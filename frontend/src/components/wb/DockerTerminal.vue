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
import { Terminal } from '@xterm/xterm'
import { FitAddon } from '@xterm/addon-fit'
import '@xterm/xterm/css/xterm.css'
import { resolveTerminalFontFamily, savedSettings, termPalette } from '../../stores/settings'

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
let ready = false
let disposed = false

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
  term.write('\x1b[2m正在连接容器…\x1b[0m\r\n')
  term.onData(writeToBackend)
  term.onResize(({ cols, rows }) => {
    if (!ready || disposed) return
    void invoke('toolbox_docker_terminal_resize', {
      sessionId: props.sessionId,
      cols,
      rows,
    }).catch(() => {})
  })

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
    term.writeln(`\x1b[31m连接失败：${text}\x1b[0m`)
    emit('error', text)
  }
})

onBeforeUnmount(() => {
  disposed = true
  observer?.disconnect()
  observer = null
  void invoke('toolbox_docker_terminal_close', { sessionId: props.sessionId }).catch(() => {})
  term?.dispose()
  term = null
  fit = null
})
</script>

<template>
  <div ref="el" class="dk-term-xterm"></div>
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
