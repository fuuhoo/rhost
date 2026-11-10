//! 会话管理器：统一管理所有存活 SSH 会话的生命周期。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use log::debug;
use tokio::sync::{RwLock, mpsc};
use uuid::Uuid;

use super::session::{DockerTerm, ExecOutput, SshSession};
use super::sftp::{RemoteDirListing, TransferPolicy, TransferProgress};
use super::{SessionConfig, SshError};
use tauri::ipc::Channel;

/// 会话池：session_id (uuid v4) → 会话句柄
pub struct SessionManager {
    sessions: Arc<RwLock<HashMap<String, Arc<SshSession>>>>,
    /// 每会话最多一个容器交互终端：session_id → 终端句柄
    docker_terms: Arc<RwLock<HashMap<String, Arc<DockerTerm>>>>,
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionManager {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            docker_terms: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// 建立新会话：连接成功后注册进会话池，返回 (session_id, 帧接收端)。
    /// 连接阶段不持锁，避免慢网络阻塞其他会话的读写。
    pub async fn create(
        &self,
        cfg: SessionConfig,
    ) -> Result<(String, mpsc::Receiver<Vec<u8>>), SshError> {
        let id = Uuid::new_v4().to_string();
        // sid 取 uuid 前 6 位（日志事件会话标签，贯穿连接全生命周期）
        let sid = id.chars().take(6).collect::<String>();
        let (session, frame_rx) = SshSession::connect(cfg, &sid).await?;
        self.sessions
            .write()
            .await
            .insert(id.clone(), Arc::new(session));
        debug!("会话注册: {id}");
        Ok((id, frame_rx))
    }

    /// 向指定会话写入前端输入（复制 Arc 后释放读锁，不持锁 await）
    pub async fn write(&self, session_id: &str, data: Vec<u8>) -> Result<(), SshError> {
        let session = {
            let sessions = self.sessions.read().await;
            sessions.get(session_id).cloned()
        };
        match session {
            Some(s) => s.write(data).await,
            None => Err(SshError::NotFound),
        }
    }

    /// 同步 PTY 尺寸：转发 window-change（复制 Arc 后释放读锁）
    pub async fn resize(&self, session_id: &str, cols: u32, rows: u32) -> Result<(), SshError> {
        let session = {
            let sessions = self.sessions.read().await;
            sessions.get(session_id).cloned()
        };
        match session {
            Some(s) => s.resize(cols, rows).await,
            None => Err(SshError::NotFound),
        }
    }

    /// 经 exec channel 执行只读命令（转发 SshSession.exec_collect）。
    /// 用于 AI ReAct 只读工具（read_file / list_directory / get_system_info）。
    pub async fn exec_collect(
        &self,
        session_id: &str,
        script: &str,
        timeout: Duration,
    ) -> Result<ExecOutput, SshError> {
        let session = {
            let sessions = self.sessions.read().await;
            sessions.get(session_id).cloned()
        };
        match session {
            Some(s) => s.exec_collect(script, timeout).await,
            None => Err(SshError::NotFound),
        }
    }

    /// 经 exec channel 执行命令并把 `stdin` 写入通道后 EOF。
    /// 工具箱的 sudo 提权用它传密码（密码不进命令行、不落日志）。
    pub async fn exec_collect_with_stdin(
        &self,
        session_id: &str,
        script: &str,
        stdin: &[u8],
        timeout: Duration,
    ) -> Result<ExecOutput, SshError> {
        let session = {
            let sessions = self.sessions.read().await;
            sessions.get(session_id).cloned()
        };
        match session {
            Some(s) => s.exec_collect_with_stdin(script, stdin, timeout).await,
            None => Err(SshError::NotFound),
        }
    }

    /// 启动容器交互终端：同会话已有终端则先关闭旧的（UI 一次只开一个）。
    /// 返回终端句柄与帧接收端，帧的转发由调用方完成。
    pub async fn open_docker_terminal(
        &self,
        session_id: &str,
        command: String,
        cols: u32,
        rows: u32,
    ) -> Result<(Arc<DockerTerm>, mpsc::Receiver<Vec<u8>>), SshError> {
        let session = {
            let sessions = self.sessions.read().await;
            sessions.get(session_id).cloned()
        };
        let Some(session) = session else {
            return Err(SshError::NotFound);
        };
        let (term, frame_rx) = session.open_docker_terminal(command, cols, rows).await?;
        let term = Arc::new(term);
        let mut terms = self.docker_terms.write().await;
        if let Some(old) = terms.insert(session_id.to_string(), term.clone()) {
            old.close();
        }
        Ok((term, frame_rx))
    }

    pub async fn docker_terminal_write(
        &self,
        session_id: &str,
        data: Vec<u8>,
    ) -> Result<(), SshError> {
        let term = self.docker_terms.read().await.get(session_id).cloned();
        match term {
            Some(t) => t.write(data).await,
            None => Err(SshError::NotFound),
        }
    }

    pub async fn docker_terminal_resize(
        &self,
        session_id: &str,
        cols: u32,
        rows: u32,
    ) -> Result<(), SshError> {
        let term = self.docker_terms.read().await.get(session_id).cloned();
        match term {
            Some(t) => t.resize(cols, rows).await,
            None => Err(SshError::NotFound),
        }
    }

    /// 关闭容器终端（从表中移除并取消子任务；会话本身不受影响）
    pub async fn close_docker_terminal(&self, session_id: &str) -> bool {
        let term = self.docker_terms.write().await.remove(session_id);
        match term {
            Some(t) => {
                t.close();
                true
            }
            None => false,
        }
    }

    /// 启动一条端口转发规则（复制 Arc 后释放读锁；业务逻辑在 TunnelManager）
    pub(crate) async fn tunnel_start(
        &self,
        session_id: &str,
        rule: super::tunnel::TunnelRule,
        opts: super::tunnel::StartOptions,
    ) -> Result<(), SshError> {
        let session = {
            let sessions = self.sessions.read().await;
            sessions.get(session_id).cloned()
        };
        match session {
            Some(s) => s.tunnel().start(rule, opts).await,
            None => Err(SshError::NotFound),
        }
    }

    /// 停止一条端口转发规则（幂等：规则不存在视为成功）
    pub(crate) async fn tunnel_stop(
        &self,
        session_id: &str,
        rule_id: &str,
    ) -> Result<(), SshError> {
        let session = {
            let sessions = self.sessions.read().await;
            sessions.get(session_id).cloned()
        };
        match session {
            Some(s) => s.tunnel().stop(rule_id).await,
            None => Err(SshError::NotFound),
        }
    }

    /// 启动某会话的动态指标采集（幂等，间隔由前端指定，单位毫秒；
    /// `iface` 为默认路由网卡，网络计数优先取它）
    pub async fn start_metrics(
        &self,
        session_id: &str,
        interval_ms: u64,
        iface: Option<String>,
    ) -> Result<(), SshError> {
        let session = { self.sessions.read().await.get(session_id).cloned() };
        match session {
            Some(s) => s.start_metrics(interval_ms, iface),
            None => Err(SshError::NotFound),
        }
    }

    /// 停止某会话的动态指标采集
    pub async fn stop_metrics(&self, session_id: &str) -> Result<(), SshError> {
        let session = { self.sessions.read().await.get(session_id).cloned() };
        match session {
            Some(s) => {
                s.stop_metrics();
                Ok(())
            }
            None => Err(SshError::NotFound),
        }
    }

    /// 前端心跳续约，维持指标采集存活
    pub async fn metrics_heartbeat(&self, session_id: &str) -> Result<(), SshError> {
        let session = { self.sessions.read().await.get(session_id).cloned() };
        match session {
            Some(s) => {
                s.metrics_heartbeat();
                Ok(())
            }
            None => Err(SshError::NotFound),
        }
    }

    /// 进程弹窗打开期间订阅全量进程列表（关闭即退订）
    pub async fn set_procs_full(&self, session_id: &str, on: bool) -> Result<(), SshError> {
        let session = { self.sessions.read().await.get(session_id).cloned() };
        match session {
            Some(s) => {
                s.set_procs_full(on);
                Ok(())
            }
            None => Err(SshError::NotFound),
        }
    }

    /// SFTP 列举远端目录：path 为 None 时返回登录默认目录（远端家目录）
    pub(crate) async fn sftp_list_dir(
        &self,
        session_id: &str,
        path: Option<String>,
    ) -> Result<RemoteDirListing, SshError> {
        let session = { self.sessions.read().await.get(session_id).cloned() };
        match session {
            Some(s) => s.sftp_list_dir(path).await,
            None => Err(SshError::NotFound),
        }
    }

    /// SFTP 创建目录
    pub(crate) async fn sftp_mkdir(&self, session_id: &str, path: String) -> Result<(), SshError> {
        let session = { self.sessions.read().await.get(session_id).cloned() };
        match session {
            Some(s) => s.sftp_mkdir(path).await,
            None => Err(SshError::NotFound),
        }
    }

    /// SFTP 批量创建目录（mkdir -p 语义，去重后一轮往返）：拖拽上传整棵目录树用
    pub(crate) async fn sftp_mkdir_all(
        &self,
        session_id: &str,
        paths: Vec<String>,
    ) -> Result<(), SshError> {
        let session = { self.sessions.read().await.get(session_id).cloned() };
        match session {
            Some(s) => s.sftp_mkdir_all(paths).await,
            None => Err(SshError::NotFound),
        }
    }

    /// SFTP 远端路径元数据（编辑器回写前的变更判定）
    pub(crate) async fn sftp_stat(
        &self,
        session_id: &str,
        path: String,
    ) -> Result<crate::ssh::sftp::RemoteStat, SshError> {
        let session = { self.sessions.read().await.get(session_id).cloned() };
        match session {
            Some(s) => s.sftp_stat(path).await,
            None => Err(SshError::NotFound),
        }
    }

    /// SFTP 递归枚举远端目录树（下载方向整棵递归，与 local_walk 对称）
    pub(crate) async fn sftp_walk(
        &self,
        session_id: &str,
        root: String,
        max_files: Option<usize>,
    ) -> Result<crate::ssh::sftp::RemoteWalkResult, SshError> {
        let session = { self.sessions.read().await.get(session_id).cloned() };
        match session {
            Some(s) => s.sftp_walk(root, max_files).await,
            None => Err(SshError::NotFound),
        }
    }

    /// SFTP 取回编辑副本（不经传输队列，无覆盖策略/续传/校验）
    pub(crate) async fn sftp_fetch(
        &self,
        session_id: &str,
        remote_path: String,
        local_path: String,
    ) -> Result<u64, SshError> {
        let session = { self.sessions.read().await.get(session_id).cloned() };
        match session {
            Some(s) => s.sftp_fetch(remote_path, local_path).await,
            None => Err(SshError::NotFound),
        }
    }

    /// 上传单个文件：本地路径 → 远端路径，行为策略由前端 `policy` 传入（见 sftp.rs），
    /// 进度经事件推送
    pub(crate) async fn sftp_upload(
        &self,
        session_id: &str,
        task_id: u64,
        local_path: String,
        remote_path: String,
        chunk_kb: Option<u32>,
        resume: Option<bool>,
        max_bytes: Option<u64>,
        policy: Option<TransferPolicy>,
        channel: Channel<TransferProgress>,
    ) -> Result<(), SshError> {
        let session = { self.sessions.read().await.get(session_id).cloned() };
        match session {
            Some(s) => {
                s.upload_file(
                    task_id,
                    local_path,
                    remote_path,
                    chunk_kb,
                    resume,
                    max_bytes,
                    policy,
                    channel,
                )
                .await
            }
            None => Err(SshError::NotFound),
        }
    }

    /// 下载单个文件：远端路径 → 本地路径（本地父目录自动创建），行为策略同上传
    pub(crate) async fn sftp_download(
        &self,
        session_id: &str,
        task_id: u64,
        remote_path: String,
        local_path: String,
        chunk_kb: Option<u32>,
        resume: Option<bool>,
        max_bytes: Option<u64>,
        policy: Option<TransferPolicy>,
        channel: Channel<TransferProgress>,
    ) -> Result<(), SshError> {
        let session = { self.sessions.read().await.get(session_id).cloned() };
        match session {
            Some(s) => {
                s.download_file(
                    task_id,
                    remote_path,
                    local_path,
                    chunk_kb,
                    resume,
                    max_bytes,
                    policy,
                    channel,
                )
                .await
            }
            None => Err(SshError::NotFound),
        }
    }

    /// SFTP → Shell 方向同步：让远端交互式 shell `cd` 到指定目录
    pub(crate) async fn sftp_sync_cwd(
        &self,
        session_id: &str,
        path: String,
    ) -> Result<(), SshError> {
        let session = { self.sessions.read().await.get(session_id).cloned() };
        match session {
            Some(s) => s.sync_shell_cd(&path).await,
            None => Err(SshError::NotFound),
        }
    }

    /// 设定会话 SFTP 通道的空闲关闭阈值（秒，0 = 不自动关闭）。
    /// 阈值由前端设置传入，Rust 侧不读 app_config.json。
    pub(crate) async fn sftp_set_idle_timeout(
        &self,
        session_id: &str,
        sec: u32,
    ) -> Result<(), SshError> {
        let session = { self.sessions.read().await.get(session_id).cloned() };
        match session {
            Some(s) => {
                s.sftp_set_idle_timeout(sec).await;
                Ok(())
            }
            None => Err(SshError::NotFound),
        }
    }

    /// SFTP 删除文件/目录（目录递归）
    pub(crate) async fn sftp_remove(&self, session_id: &str, path: String) -> Result<(), SshError> {
        let session = { self.sessions.read().await.get(session_id).cloned() };
        match session {
            Some(s) => s.sftp_remove_path(path).await,
            None => Err(SshError::NotFound),
        }
    }

    /// SFTP 重命名/移动
    pub(crate) async fn sftp_rename(
        &self,
        session_id: &str,
        old_path: String,
        new_path: String,
    ) -> Result<(), SshError> {
        let session = { self.sessions.read().await.get(session_id).cloned() };
        match session {
            Some(s) => s.sftp_rename_path(old_path, new_path).await,
            None => Err(SshError::NotFound),
        }
    }

    /// SFTP 创建空文件
    pub(crate) async fn sftp_create_file(
        &self,
        session_id: &str,
        path: String,
    ) -> Result<(), SshError> {
        let session = { self.sessions.read().await.get(session_id).cloned() };
        match session {
            Some(s) => s.sftp_create_file(path).await,
            None => Err(SshError::NotFound),
        }
    }

    /// SFTP 同主机复制（目录递归）
    pub(crate) async fn sftp_copy(
        &self,
        session_id: &str,
        src: String,
        dst: String,
    ) -> Result<(), SshError> {
        let session = { self.sessions.read().await.get(session_id).cloned() };
        match session {
            Some(s) => s.sftp_copy_path(src, dst).await,
            None => Err(SshError::NotFound),
        }
    }

    /// SFTP 读取符号链接目标
    pub(crate) async fn sftp_readlink(
        &self,
        session_id: &str,
        path: String,
    ) -> Result<String, SshError> {
        let session = { self.sessions.read().await.get(session_id).cloned() };
        match session {
            Some(s) => s.sftp_readlink(path).await,
            None => Err(SshError::NotFound),
        }
    }

    /// SFTP 修改权限（chmod，可选递归；`scope` = all / files / dirs）
    pub(crate) async fn sftp_set_perm(
        &self,
        session_id: &str,
        path: String,
        mode: u32,
        recursive: bool,
        scope: String,
    ) -> Result<(), SshError> {
        let session = { self.sessions.read().await.get(session_id).cloned() };
        match session {
            Some(s) => s.sftp_set_perm(path, mode, recursive, scope).await,
            None => Err(SshError::NotFound),
        }
    }

    /// 暂停/继续某传输任务
    pub(crate) async fn sftp_transfer_pause(
        &self,
        session_id: &str,
        task_id: u64,
        paused: bool,
    ) -> Result<(), SshError> {
        let session = { self.sessions.read().await.get(session_id).cloned() };
        match session {
            Some(s) => s.transfer_pause(task_id, paused).await,
            None => Err(SshError::NotFound),
        }
    }

    /// 取消某传输任务
    pub(crate) async fn sftp_transfer_cancel(
        &self,
        session_id: &str,
        task_id: u64,
    ) -> Result<(), SshError> {
        let session = { self.sessions.read().await.get(session_id).cloned() };
        match session {
            Some(s) => s.transfer_cancel(task_id).await,
            None => Err(SshError::NotFound),
        }
    }

    /// 断开并移除会话：触发取消令牌，后台任务自行收尾退出
    pub async fn disconnect(&self, session_id: &str) -> bool {
        let term = self.docker_terms.write().await.remove(session_id);
        if let Some(t) = term {
            t.close();
        }
        let session = self.sessions.write().await.remove(session_id);
        if let Some(s) = session {
            debug!("会话注销: {session_id}");
            s.shutdown();
            true
        } else {
            false
        }
    }

    /// 关闭全部会话（应用退出时调用），返回关闭数量
    pub async fn shutdown_all(&self) -> usize {
        for (_, t) in self.docker_terms.write().await.drain() {
            t.close();
        }
        let mut sessions = self.sessions.write().await;
        let count = sessions.len();
        for (_, s) in sessions.drain() {
            s.shutdown();
        }
        count
    }

    /// 当前存活会话数（退出事件 kv 用）
    pub async fn session_count(&self) -> usize {
        self.sessions.read().await.len()
    }
}
