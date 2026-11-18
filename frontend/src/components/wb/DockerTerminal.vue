<script setup lang="ts">
/**
 * 容器交互终端：独立 xterm 实例 + 专用二进制帧通道。
 *
 * 与主终端 TerminalPane 完全隔离（不复用单例、不参与断线重连）：打开时
 * 由父组件传入 `openSession`（内部处理 sudo 提权），后端在该会话上另开
 * 一条 SSH 通道跑 `docker exec -it`。关闭浮层即关闭该终端，不影响会话。
 */
import { onBeforeUnmount, onMounted, ref } from 'vue'
import { Channel, invoke } from '@tauri-apps/api/core'
import { readText, writeText } from '@tauri-apps/plugin-clipboard-manager'
import { Terminal } from '@xterm/xterm'
import { FitAddon } from '@xterm/addon-fit'
import '@xterm/xterm/css/xterm.css'
import { resolveTerminalFontFamily, savedSettings, termPalette } from '../../stores/settings'
import { confirmDialog } from '../../composables/useConfirm'
import ContextMenu, { type MenuItem } from './ContextMenu.vue'

export type DockerTermChannel = Channel<number[] | ArrayBuffer | Uint8Array>

const props = defineProps<{
  sessionId: string
  container: string
  /** 由父组件注入：创建通道并完成 open invoke（含 sudo 重试） */
  openSession: (channel: DockerTermChannel, cols: number, rows: number) => Promise<void>
}>()

const emit = defineEmits<{
  (e: 'error', message: string): void
  (e: 'exit', reason: string): void
}>()

const el = ref<HTMLElement | null>(null)

let term: Terminal | null = null
let fit: FitAddon | null = null
let observer: ResizeObserver | null = null
let detachDom: (() => void) | null = null
let ready = false
let disposed = false
/** 首帧数据前光标停在「正在连接容器…」行末，首帧到达时擦掉该提示再写数据 */
let firstData = true

const FRAME_DATA = 0x01
const FRAME_EXIT = 0x02

type Frame = { type: number; payload: Uint8Array }

/** 二进制流按「1B 类型 + 4B 大端长度 + payload」切帧；跨包残片暂存 */
let pending = new Uint8Array(0)

function decodeFrames(chunk: Uint8Array): Frame[] {
  const buf = new Uint8Array(pending.length + chunk.length)
  buf.set(pending)
  buf.set(chunk, pending.length)
  const frames: Frame[] = []
  let off = 0
  while (buf.length - off >= 5) {
    const type = buf[off]
    const len = new DataView(buf.buffer, buf.byteOffset + off + 1, 4).getUint32(0)
    if (buf.length - off - 5 < len) break
    frames.push({ type, payload: buf.slice(off + 5, off + 5 + len) })
    off += 5 + len
  }
  pending = buf.slice(off)
  return frames
}

function onFrame(bytes: Uint8Array) {
  if (!term || disposed) return
  for (const frame of decodeFrames(bytes)) {
    if (frame.type === FRAME_DATA) {
      if (firstData) {
        firstData = false
        term.write('\r\x1b[2K')
      }
      term.write(frame.payload)
    } else if (frame.type === FRAME_EXIT) {
      ready = false
      let reason = '容器终端已结束'
      try {
        const parsed = JSON.parse(new TextDecoder().decode(frame.payload)) as {
          code?: number | null
          reason?: string | null
        }
        if (parsed.reason) reason = parsed.reason
        else if (parsed.code != null) reason = `退出码 ${parsed.code}`
      } catch {
        // 非 JSON 负载按默认文案
      }
      term.writeln(`\r\n\x1b[2m[${reason}]\x1b[0m`)
      emit('exit', reason)
    }
  }
}

function writeToBackend(data: string) {
  if (!ready || disposed) return
  void invoke('toolbox_docker_terminal_write', { sessionId: props.sessionId, data }).catch(() => {
    // 终端已关闭（用户退出容器/会话断开）：输入无处可去，静默丢弃
  })
}

function syncSize() {
  if (!fit || !term || disposed) return
  try {
    fit.fit()
  } catch {
    // 容器不可见（浮层刚关闭）时 fit 可能抛错，忽略
  }
}

/* ---- 右键菜单 / 粘贴（与主终端同一套行为与设置）---- */
const menuOpen = ref(false)
const menuPos = ref({ x: 0, y: 0 })
const menuItems = ref<MenuItem[]>([])

const ICON_COPY = 'M9 9h10a2 2 0 0 1 2 2v10a2 2 0 0 1-2 2H11a2 2 0 0 1-2-2V9zM5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1'
const ICON_PASTE = 'M16 4h2a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2h2M9 2h6a1 1 0 0 1 1 1v2a1 1 0 0 1-1 1H9a1 1 0 0 1-1-1V3a1 1 0 0 1 1-1z'
const ICON_CLEAR = 'M18 6L6 18M6 6l12 12'
const ICON_SELECT = 'M8 3H5a2 2 0 0 0-2 2v3m18 0V5a2 2 0 0 0-2-2h-3m0 18h3a2 2 0 0 0 2-2v-3M3 16v3a2 2 0 0 0 2 2h3'

const PASTE_CTRL_RE = /[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]/

function copySelection() {
  const text = term?.getSelection()
  if (!text) return
  const out = savedSettings.trimOnCopy ? text.replace(/[ \t]+$/gm, '') : text
  writeText(out).catch(() => {})
}

/** 多行/控制字符粘贴确认（设置 pasteGuard 开启时），与主终端一致 */
async function pasteWithGuard(text: string) {
  if (savedSettings.pasteGuard && (/\r\n|\r|\n/.test(text) || PASTE_CTRL_RE.test(text))) {
    const ok = await confirmDialog(
      '剪贴板内容包含换行或不可见控制字符，粘贴后可能直接执行多条命令。确认粘贴？',
      '确认粘贴',
      { confirmText: '确认粘贴' },
    )
    if (!ok || disposed) return
  }
  if (!disposed) term?.paste(text)
}

function pasteFromClipboard() {
  readText().then(text => {
    if (text) void pasteWithGuard(text)
  }).catch(() => {})
}

function openMenu(x: number, y: number) {
  const t = term
  if (!t) return
  menuItems.value = [
    { label: '复制', icon: ICON_COPY, shortcut: '⌘C', disabled: !t.hasSelection(), action: copySelection },
    { label: '粘贴', icon: ICON_PASTE, shortcut: '⌘V', action: pasteFromClipboard },
    { divider: true },
    { label: '清屏', icon: ICON_CLEAR, action: () => t.clear() },
    { label: '全选', icon: ICON_SELECT, shortcut: '⌘A', action: () => t.selectAll() },
  ]
  menuPos.value = { x, y }
  menuOpen.value = true
}

onMounted(async () => {
  const palette = termPalette.value
  term = new Terminal({
    cursorBlink: savedSettings.cursorBlink,
    cursorStyle: savedSettings.cursorStyle === 'block' || savedSettings.cursorStyle === 'underline'
      ? savedSettings.cursorStyle
      : 'bar',
    fontSize: savedSettings.fontSize,
    fontFamily: resolveTerminalFontFamily(savedSettings.fontFamily),
    lineHeight: Math.max(1, savedSettings.lineHeight / 100),
    scrollback: Math.max(0, Math.trunc(savedSettings.scrollback) || 0),
    theme: {
      background: palette.background,
      foreground: palette.foreground,
      cursor: palette.cursor,
      cursorAccent: palette.cursorAccent,
      selectionBackground: palette.selectionBackground,
      ...palette.ansi,
    },
  })
  fit = new FitAddon()
  term.loadAddon(fit)
  if (el.value) {
    term.open(el.value)
    fit.fit()
  }
  term.write('\x1b[2m正在连接容器…\x1b[0m')
  term.onData(writeToBackend)
  term.onResize(({ cols, rows }) => {
    if (!ready || disposed) return
    void invoke('toolbox_docker_terminal_resize', {
      sessionId: props.sessionId,
      cols,
      rows,
    }).catch(() => {})
  })

  // 右键：menu 模式弹菜单，paste 模式直接粘贴（与主终端 settings.rightClick 一致）
  const screenEl = el.value
  const onCtx = (ev: MouseEvent) => {
    ev.preventDefault()
    ev.stopPropagation()
    if (savedSettings.rightClick !== 'menu') {
      pasteFromClipboard()
      return
    }
    openMenu(ev.clientX, ev.clientY)
  }
  // 接管 Cmd+V：先过粘贴保护再交给 xterm 的 bracketed-paste 流程
  const onPaste = (ev: ClipboardEvent) => {
    const text = ev.clipboardData?.getData('text/plain')
    if (!text) return
    ev.preventDefault()
    ev.stopPropagation()
    void pasteWithGuard(text)
  }
  const onCopy = (ev: ClipboardEvent) => {
    if (!savedSettings.trimOnCopy || !term?.hasSelection()) return
    ev.stopPropagation()
    ev.preventDefault()
    ev.clipboardData?.setData('text/plain', term.getSelection().replace(/[ \t]+$/gm, ''))
  }
  if (screenEl) {
    screenEl.addEventListener('contextmenu', onCtx, true)
    screenEl.addEventListener('paste', onPaste, true)
    screenEl.addEventListener('copy', onCopy, true)
    detachDom = () => {
      screenEl.removeEventListener('contextmenu', onCtx, true)
      screenEl.removeEventListener('paste', onPaste, true)
      screenEl.removeEventListener('copy', onCopy, true)
    }
  }

  observer = new ResizeObserver(() => syncSize())
  if (el.value) observer.observe(el.value)

  const channel = new Channel<number[] | ArrayBuffer | Uint8Array>()
  channel.onmessage = raw => {
    const bytes = raw instanceof ArrayBuffer
      ? new Uint8Array(raw)
      : raw instanceof Uint8Array
        ? raw
        : new Uint8Array(raw)
    onFrame(bytes)
  }

  try {
    await props.openSession(channel, term.cols, term.rows)
    if (disposed) return
    ready = true
    term.focus()
  } catch (e) {
    const msg = typeof e === 'string' ? e : e instanceof Error ? e.message : String(e)
    const text = msg === '已取消输入 sudo 密码' ? '已取消打开容器终端' : msg
    if (firstData) {
      firstData = false
      term.write('\r\x1b[2K')
    }
    term.writeln(`\x1b[31m连接失败：${text}\x1b[0m`)
    emit('error', text)
  }
})

onBeforeUnmount(() => {
  disposed = true
  observer?.disconnect()
  observer = null
  detachDom?.()
  detachDom = null
  void invoke('toolbox_docker_terminal_close', { sessionId: props.sessionId }).catch(() => {})
  term?.dispose()
  term = null
  fit = null
})
</script>

<template>
  <div ref="el" class="dk-term-xterm"></div>
  <ContextMenu v-model:visible="menuOpen" :x="menuPos.x" :y="menuPos.y" :items="menuItems" />
</template>

<style scoped>
.dk-term-xterm {
  flex: 1;
  min-height: 0;
  height: 100%;
  width: 100%;
  padding: 6px 4px 4px 8px;
  overflow: hidden;
}
.dk-term-xterm :deep(.xterm) {
  height: 100%;
}
.dk-term-xterm :deep(.xterm-viewport) {
  background: transparent !important;
}
</style>
