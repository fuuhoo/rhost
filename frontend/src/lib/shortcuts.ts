/**
 * 设置驱动的全局快捷键（设置 → 快捷键）。
 *
 * 绑定值由 SettingsModal 的录入器写成展示串：修饰键标签 ⌘/Ctrl/Alt/Shift + 主键，
 * 以 '+' 连接（如 '⌘+T'、'Ctrl+Shift+B'、'⌘+,'）。本模块负责解析并派发。
 *
 * 与组件内硬编码监听的分工：按键输入本身仍由 xterm 处理，这里只接命令类快捷键；
 * TerminalPane 仅保留广播条（⌘⇧B）这类与组件内部状态强绑定的快捷键。
 */
import { savedSettings, showSettings } from '../stores/settings'
import { showNewConn } from '../stores/hosts'
import { activeSessionId, closeSession, openBlankSession } from '../stores/session'
import { getTerm } from './termRegistry'
import { showPalette, termFindTick } from './uiBus'

/** 设置项 key → 动作。分屏两项不在此列：工作台尚无分屏 UI，无从绑定 */
const ACTIONS: Record<string, () => void> = {
  'key.newTab': () => {
    showNewConn.value = true
  },
  // FinalShell「新建终端窗口」（Ctrl+Shift+T）同款：开一个未连接的空白终端标签，
  // 随后双击主机列表即连进该标签（见 session store openSession 的空白标签转正）
  'key.newWindow': () => {
    openBlankSession()
  },
  'key.closeTab': () => {
    const id = activeSessionId.value
    if (id) closeSession(id)
  },
  'key.clear': () => {
    const id = activeSessionId.value
    // Terminal.clear() 同时清屏与清空 scrollback（xterm 原生语义）
    getTerm(id ?? '')?.clear()
  },
  'key.palette': () => {
    showPalette.value = true
  },
  'key.find': () => {
    termFindTick.value++
  },
  'key.settings': () => {
    showSettings.value = true
  },
}

interface Combo {
  meta: boolean
  ctrl: boolean
  alt: boolean
  shift: boolean
  /** 已归一化（小写）的主键，与 KeyboardEvent.key 同域 */
  key: string
}

/** 展示串 → 修饰键 + 主键；空串/缺主键返回 null（未绑定） */
export function parseCombo(raw: unknown): Combo | null {
  if (typeof raw !== 'string') return null
  const parts = raw.split('+').map(p => p.trim()).filter(p => p !== '')
  if (parts.length === 0) return null
  const mods = parts.slice(0, -1)
  const main = parts[parts.length - 1] as string
  const key = normalizeKey(main)
  if (!key) return null
  return {
    meta: mods.some(m => m === '⌘' || m.toLowerCase() === 'meta' || m.toLowerCase() === 'cmd'),
    ctrl: mods.some(m => m.toLowerCase() === 'ctrl' || m.toLowerCase() === 'control'),
    alt: mods.some(m => m.toLowerCase() === 'alt' || m === '⌥'),
    shift: mods.some(m => m.toLowerCase() === 'shift'),
    key,
  }
}

/** 主键归一化：统一小写（录入器把单字符存成大写），空格用 ' ' 与 KeyboardEvent.key 对齐 */
function normalizeKey(k: string): string {
  return k === 'Space' ? ' ' : k.toLowerCase()
}

/** 事件是否命中绑定串；修饰键严格相等（未录 Shift 的组合不按 Shift 触发） */
export function comboMatches(e: KeyboardEvent, raw: unknown): boolean {
  const c = parseCombo(raw)
  if (!c) return false
  if ((e.key === ' ' ? ' ' : e.key.toLowerCase()) !== c.key) return false
  return e.metaKey === c.meta && e.ctrlKey === c.ctrl && e.altKey === c.alt && e.shiftKey === c.shift
}

/** 表单输入态判定：xterm 的辅助 textarea 是终端本体，不算表单输入 */
function isFormTarget(t: EventTarget | null): boolean {
  const el = t as HTMLElement | null
  if (!el || !el.tagName) return false
  return (
    el.tagName === 'INPUT' ||
    el.tagName === 'SELECT' ||
    el.tagName === 'TEXTAREA' && !el.classList.contains('xterm-helper-textarea') ||
    el.isContentEditable === true
  )
}

function onKeydown(e: KeyboardEvent) {
  if (isFormTarget(e.target)) return
  for (const [settingKey, run] of Object.entries(ACTIONS)) {
    const combo = (savedSettings as Record<string, unknown>)[settingKey]
    if (comboMatches(e, combo)) {
      e.preventDefault()
      run()
      return
    }
  }
}

/** 安装/卸载全局快捷键监听（App.vue onMounted/onUnmounted 各调一次） */
export function installGlobalShortcuts() {
  document.addEventListener('keydown', onKeydown)
  return () => document.removeEventListener('keydown', onKeydown)
}
