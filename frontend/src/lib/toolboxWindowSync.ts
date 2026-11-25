/**
 * 主窗口 → 独立工具箱窗口的「会话存活」广播。
 *
 * 独立窗口盯的是建窗时那一条 SSH 会话（backendId）。标签被关掉，或断线重连后换了新的
 * backendId，这条会话就不在了——窗口里的命令只会一个个报「会话不存在」。主窗口在会话
 * 集合变化时把当前存活的后端会话 id 广播出去，独立窗口发现自己盯的那条不在列表里就
 * 自行关窗，不留一个怎么点都失败的僵尸窗口。
 *
 * 走 Tauri 事件（core:event:default 已含 emit / listen，见 capabilities/default.json）。
 */
import { watch } from 'vue'
import { emit, listen, type UnlistenFn } from '@tauri-apps/api/event'
import { sessions } from '../stores/session'
import { isTauri } from './tauri'

const LIVE_SESSIONS_EVENT = 'toolbox:live-sessions'

/** 主窗口调用：会话集合变化时广播存活的后端会话 id；返回停止函数 */
export function startToolboxSessionBroadcast(): UnlistenFn {
  if (!isTauri) return () => {}
  return watch(
    () => sessions.value.map(s => s.backendId).filter((id): id is string => !!id),
    ids => {
      void emit(LIVE_SESSIONS_EVENT, ids)
    },
  )
}

/** 独立窗口调用：自己盯的会话从存活列表里消失时触发回调（用于关窗提示） */
export async function onToolboxSessionGone(
  sessionId: string,
  cb: () => void,
): Promise<UnlistenFn> {
  if (!isTauri) return () => {}
  return listen<string[]>(LIVE_SESSIONS_EVENT, ev => {
    const ids = ev.payload ?? []
    if (!ids.includes(sessionId)) cb()
  })
}
