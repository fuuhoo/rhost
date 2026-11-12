import { writeText } from '@tauri-apps/plugin-clipboard-manager'
import { activeSessionId } from '../stores/session'
import { getTerm } from './termRegistry'

/** 页面（非终端）当前选区文本；无选区或选区已折叠返回空串 */
function pageSelectionText(): string {
  const sel = window.getSelection()
  if (!sel || sel.isCollapsed) return ''
  return sel.toString()
}

/** 焦点在终端里且终端自身有选区：交给 xterm 的 copy 流程，不抢 */
function terminalSelectionActive(): boolean {
  const active = document.activeElement
  if (!active || !active.closest('.xterm')) return false
  return getTerm(activeSessionId.value ?? '')?.hasSelection() ?? false
}

/**
 * 全局复制兜底：终端持有焦点时，原生 ⌘C 的 copy 事件落在 xterm 的辅助 textarea
 * 上，只会复制终端选区，页面其它区域（监控面板、设置说明等）的选区被忽略。
 * 这里在捕获阶段接管——页面存在非空选区、且不是在复制终端自己的选区时，由前端
 * 写剪贴板并阻止事件下传。页面无选区时放行，终端选区仍走 xterm 的流程
 *（含 trimOnCopy 修剪）。
 */
function onKeydown(e: KeyboardEvent) {
  if (e.key.toLowerCase() !== 'c' || !(e.metaKey || e.ctrlKey) || e.altKey || e.shiftKey) return
  const text = pageSelectionText()
  if (!text || terminalSelectionActive()) return
  e.preventDefault()
  e.stopPropagation()
  writeText(text).catch(() => {
    void navigator.clipboard?.writeText(text).catch(() => {})
  })
}

/** 安装/卸载页面选区复制（App.vue onMounted/onBeforeUnmount 各调一次） */
export function installPageCopy() {
  window.addEventListener('keydown', onKeydown, true)
  return () => window.removeEventListener('keydown', onKeydown, true)
}