/** 是否运行在 Tauri 桌面环境（浏览器 dev 模式为 false） */
export const isTauri = typeof window !== 'undefined' && '__TAURI_INTERNALS__' in window

export type AppPlatform = 'macos' | 'windows' | 'linux' | 'unknown'

/**
 * 同步探测运行平台。优先读 lib.rs 通过 initialization_script 写入的
 * window.__RHOST_BOOT__.platform（任意前端脚本执行前即就绪，与首帧渲染无竞态）；
 * 浏览器 dev 模式回退到 navigator.userAgent 判定。
 *
 * 用于：标题栏安全区方向（macOS 左侧为原生红绿灯预留）、
 * 是否渲染前端自绘窗口控制按钮（Windows/Linux 右侧）。
 */
export function detectPlatform(): AppPlatform {
  const boot = (window as unknown as { __RHOST_BOOT__?: { platform?: string } }).__RHOST_BOOT__
  const p = boot?.platform
  if (p === 'macos' || p === 'windows' || p === 'linux') return p
  const ua = navigator.userAgent.toLowerCase()
  if (ua.includes('mac')) return 'macos'
  if (ua.includes('windows')) return 'windows'
  if (ua.includes('linux')) return 'linux'
  return 'unknown'
}

/** 获取当前窗口实例；浏览器环境返回 null（调用方需判空） */
export async function appWindow() {
  if (!isTauri) return null
  const { getCurrentWindow } = await import('@tauri-apps/api/window')
  return getCurrentWindow()
}

/**
 * 当前应用版本号（不含「开发版」后缀），如 `2.3.0`；浏览器 dev 预览无注入时为空串。
 * 与 Cargo.toml / tauri.conf.json / frontend/package.json 同源。
 */
export const appVersion: string = (() => {
  const boot = (window as unknown as { __RHOST_BOOT__?: { appVersion?: string } }).__RHOST_BOOT__
  return boot?.appVersion ?? ''
})()

/**
 * 应用版本展示串，如「2.3.0 · 开发版」（开发版仅在 debug 构建下出现）。
 * 取 lib.rs 首帧注入的 window.__RHOST_BOOT__，与 Cargo.toml / tauri.conf.json /
 * frontend/package.json 的版本同源；浏览器 dev 预览无注入时为空串，调用方需隐藏该行。
 */
export const appVersionLabel: string = (() => {
  const boot = (window as unknown as {
    __RHOST_BOOT__?: { appVersion?: string; appDevBuild?: boolean }
  }).__RHOST_BOOT__
  const v = boot?.appVersion
  if (!v) return ''
  return boot?.appDevBuild ? `${v} · 开发版` : v
})()

/* ---- 工具箱独立窗口 ---- */

/** 独立窗口的会话信息：Rust 建窗时经首帧注入 `window.__RHOST_TOOLBOX__` 传进来，
 *  省掉一次 IPC（也可以避免窗口先渲染成空壳再填内容） */
export interface ToolboxWindowMeta {
  /** 后端 SSH 会话 id（connect_ssh 返回的 uuid） */
  sessionId: string
  /** 主机展示名（标题副标题与窗口标题） */
  label: string
  user: string
  port: number
  host: string
}

/** 当前窗口是否是工具箱独立窗口：是则返回注入的会话信息，主窗口返回 null */
export function toolboxWindowMeta(): ToolboxWindowMeta | null {
  const meta = (window as unknown as { __RHOST_TOOLBOX__?: ToolboxWindowMeta })
    .__RHOST_TOOLBOX__
  return meta?.sessionId ? meta : null
}

/**
 * 把当前会话的工具箱提升为独立窗口。
 * 同一会话重复调用只聚焦已有窗口（label 按会话固定，由 Rust 侧保证），
 * 多个会话可各自开一个；会话本身不复制，两边命令打到同一条 SSH 会话上。
 */
export async function openToolboxWindow(meta: ToolboxWindowMeta): Promise<void> {
  if (!isTauri) return
  const { invoke } = await import('@tauri-apps/api/core')
  await invoke('open_toolbox_window', {
    sessionId: meta.sessionId,
    label: meta.label,
    user: meta.user,
    port: meta.port,
    host: meta.host,
  })
}
