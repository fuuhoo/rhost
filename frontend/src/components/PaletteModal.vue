<script setup lang="ts">
/**
 * 全局命令面板（⌘K / 标题栏「命令」按钮唤起）。
 *
 * 定位：把散落在标题栏、右键菜单里的命令收敛为一个可键盘驱动的入口，
 * 项与项的动作全部复用 store 既有函数，本组件只做收集、过滤与派发。
 * 内置项固定（COMMANDS），末尾再拼一节用户自定义「命令模板」（设置里维护，写入终端执行）。
 * 可用性（enabled）随会话状态实时计算，禁用项保留在列表里但跳过键盘导航。
 */
import { computed, nextTick, onMounted, onUnmounted, ref, watch } from 'vue'
import { showPalette, settingsFocusPanel, termFindTick } from '../lib/uiBus'
import { getTerm } from '../lib/termRegistry'
import { toast } from '../composables/useToast'
import { savedSettings, showSettings } from '../stores/settings'
import { showNewConn } from '../stores/hosts'
import {
  activeSession,
  activeSessionId,
  appView,
  closeAllSessions,
  closeSession,
  connected,
  dockVisible,
  isExternalSession,
  monitorVisible,
  openDock,
  rightPanelVisible,
  searchTick,
  sendInput,
  sessions,
  toggleDock,
} from '../stores/session'

interface Command {
  id: string
  title: string
  /** 检索用同义词（中文场景下按拼音/英文别名也能命中） */
  kw?: string
  /** 绑定的设置项快捷键 key（右侧提示实时取值，改绑后不用改这里） */
  binding?: string
  /** 静态右侧提示（命令模板用它回显命令原文） */
  hint?: string
  group: string
  enabled?: () => boolean
  run: () => void
}

const COMMANDS: Command[] = [
  { id: 'newConn', group: '会话', title: '新建连接…', kw: 'new connection add host', binding: 'key.newTab', run: () => { showNewConn.value = true } },
  {
    id: 'closeTab',
    group: '会话',
    title: '关闭当前标签页',
    kw: 'close tab',
    binding: 'key.closeTab',
    enabled: () => !!activeSessionId.value,
    run: () => { const id = activeSessionId.value; if (id) closeSession(id) },
  },
  {
    id: 'closeAll',
    group: '会话',
    title: '关闭全部标签页',
    kw: 'close all tabs',
    enabled: () => sessions.value.length > 0,
    run: () => closeAllSessions(),
  },
  {
    id: 'clear',
    group: '会话',
    title: '清空屏幕',
    kw: 'clear screen scrollback',
    binding: 'key.clear',
    enabled: () => !!activeSessionId.value,
    run: () => { getTerm(activeSessionId.value ?? '')?.clear() },
  },
  { id: 'find', group: '会话', title: '在终端中查找…', kw: 'find search grep 搜索', binding: 'key.find', run: () => { termFindTick.value++ } },

  { id: 'viewHome', group: '界面', title: '回到主机列表', kw: 'home hosts list', run: () => { appView.value = 'home' } },
  {
    id: 'viewWb',
    group: '界面',
    title: '打开工作台',
    kw: 'workbench terminal',
    enabled: () => sessions.value.length > 0,
    run: () => { appView.value = 'workbench' },
  },
  { id: 'monitor', group: '界面', title: '切换左侧监控面板', kw: 'monitor 监控 侧栏 toggle', run: () => { monitorVisible.value = !monitorVisible.value } },
  { id: 'dock', group: '界面', title: '切换底部面板', kw: 'dock toggle 面板', run: () => toggleDock() },
  {
    id: 'dockSftp',
    group: '界面',
    title: '打开「文件」面板（SFTP）',
    kw: 'sftp files 文件',
    // ssh:// 外部连接的唯一通道是 PTY，SFTP 要另开通道（后端也会拒绝）
    enabled: () => connected.value && activeSession.value?.host.external !== 'terminal',
    run: () => openDock('sftp'),
  },
  {
    id: 'dockLog',
    group: '界面',
    title: '打开会话日志面板',
    kw: 'log logs 日志',
    enabled: () => dockVisible.value,
    run: () => openDock('log'),
  },
  {
    id: 'dockTunnel',
    group: '界面',
    title: '打开端口转发面板',
    kw: 'tunnel 隧道 转发',
    // 外部连接单通道：端口转发同样是额外通道，面板入口对这类会话禁用
    enabled: () => !isExternalSession(activeSession.value),
    run: () => openDock('tunnel'),
  },
  {
    id: 'rightPanel',
    group: '界面',
    title: '切换右侧面板（主机 / AI）',
    kw: 'ai assistant panel 助手 主机 host 右侧',
    run: () => { rightPanelVisible.value = !rightPanelVisible.value },
  },
  { id: 'logSearch', group: '界面', title: '搜索会话日志', kw: 'search log 日志搜索', run: () => { searchTick.value++ } },
  { id: 'settings', group: '应用', title: '打开设置…', kw: 'settings preferences 偏好', binding: 'key.settings', run: () => { showSettings.value = true } },
]

/** 用户自定义命令模板 → 面板条目：全局一份，选中即把命令写入当前活动终端并回车执行。
 *  空列表时保留一条「去添加」条目，让本功能在唯一的入口处可见。 */
const templateCommands = computed<Command[]>(() => {
  const list = savedSettings.commandTemplates.filter(t => t.command.trim() !== '')
  if (!list.length) {
    return [
      {
        id: 'tpl-empty',
        group: '命令模板',
        title: '还没有自定义命令 · 打开「设置 → 命令模板」添加',
        kw: 'template 命令模板 自定义 添加 shell',
        run: () => {
          settingsFocusPanel.value = 'commands'
          showSettings.value = true
        },
      },
    ]
  }
  return list.map((t, i) => ({
    id: `tpl-${i}`,
    group: '命令模板',
    title: t.name.trim() || t.command.trim(),
    kw: `${t.name} ${t.command} shell 命令模板`,
    hint: t.command.trim(),
    enabled: () => !!activeSession.value?.backendId && activeSession.value?.state === 'online',
    run: () => void runTemplate(t.command),
  }))
})

/** 写入终端并回车：多行模板用 bracketed paste 整块提交，
 *  否则中间换行会被 shell 逐行执行（与 AI 面板 writeExec 同一处理） */
async function runTemplate(raw: string) {
  const s = activeSession.value
  const code = raw.replace(/\n+$/, '')
  if (!s?.backendId || s.state !== 'online') {
    toast('无可用会话，命令未执行', 'err')
    return
  }
  const payload = code.includes('\n') ? `\x1b[200~${code}\x1b[201~\n` : `${code}\n`
  try {
    await sendInput(s.id, payload)
  } catch (e) {
    toast(`写入终端失败：${String((e as Error)?.message ?? e)}`, 'err', 3200)
    return
  }
  toast('命令已发送到终端', 'ok', 1500)
}

/** 右侧快捷键提示：直接回显设置里的绑定串（改绑即时同步） */
function hintOf(c: Command): string {
  if (c.hint) return c.hint
  if (!c.binding) return ''
  const raw = (savedSettings as Record<string, unknown>)[c.binding]
  return typeof raw === 'string' ? raw : ''
}

const query = ref('')
const inputEl = ref<HTMLInputElement | null>(null)
const listEl = ref<HTMLElement | null>(null)
const cursor = ref(0)

/** 过滤：标题或关键词包含全部空格分词即命中 */
const filtered = computed(() => {
  const all = [...COMMANDS, ...templateCommands.value]
  const q = query.value.trim().toLowerCase()
  if (!q) return all
  const terms = q.split(/\s+/)
  return all.filter(c => {
    const hay = `${c.title} ${c.kw ?? ''} ${c.group}`.toLowerCase()
    return terms.every(t => hay.includes(t))
  })
})

/** 可执行项（禁用项保留展示但键盘跳过） */
const enabledList = computed(() => filtered.value.filter(c => c.enabled?.() !== false))

function isDisabled(c: Command): boolean {
  return c.enabled?.() === false
}

/** 光标越界时收敛（改查询词/禁用态变化后） */
watch([filtered, enabledList], () => {
  const ids = enabledList.value.map(c => c.id)
  if (!ids.length) return
  if (!ids.includes(filtered.value[cursor.value]?.id ?? '')) {
    const firstEnabled = filtered.value.findIndex(c => !isDisabled(c))
    cursor.value = firstEnabled < 0 ? 0 : firstEnabled
  }
})

function openPalette() {
  query.value = ''
  cursor.value = 0
  // 懒挂载时 immediate watch 跑在 setup 期（ref 尚空），用宏任务确保挂载后聚焦
  setTimeout(() => { inputEl.value?.focus(); inputEl.value?.select() }, 0)
}
watch(showPalette, v => { if (v) openPalette() })
// 懒挂载：开屏初始化放 onMounted（setup 完成后），immediate watch 会引用后声明的 const 触发 TDZ
onMounted(() => { if (showPalette.value) openPalette() })

function move(delta: number) {
  const list = filtered.value
  if (!list.length) return
  let i = cursor.value
  for (let step = 0; step < list.length; step++) {
    i = (i + delta + list.length) % list.length
    if (!isDisabled(list[i]!)) break
  }
  cursor.value = i
  void nextTick(() => {
    listEl.value?.querySelector('[data-cursor="true"]')?.scrollIntoView({ block: 'nearest' })
  })
}

function runAt(i: number) {
  const c = filtered.value[i]
  if (!c || isDisabled(c)) return
  showPalette.value = false
  c.run()
}

function onKey(e: KeyboardEvent) {
  if (!showPalette.value) return
  if (e.key === 'Escape') {
    e.preventDefault()
    showPalette.value = false
  } else if (e.key === 'ArrowDown' || (e.ctrlKey && e.key.toLowerCase() === 'n')) {
    e.preventDefault()
    move(1)
  } else if (e.key === 'ArrowUp' || (e.ctrlKey && e.key.toLowerCase() === 'p')) {
    e.preventDefault()
    move(-1)
  } else if (e.key === 'Enter') {
    e.preventDefault()
    runAt(cursor.value)
  }
}

onMounted(() => document.addEventListener('keydown', onKey, true))
onUnmounted(() => document.removeEventListener('keydown', onKey, true))
</script>

<template>
  <div class="mask" :class="{ show: showPalette }" @click.self="showPalette = false">
    <div class="modal palette-modal" role="dialog" aria-modal="true" aria-labelledby="paletteTitle">
      <div class="palette-head">
        <h2 id="paletteTitle" class="sr-only">命令面板</h2>
        <input
          ref="inputEl"
          v-model="query"
          class="palette-input"
          type="text"
          placeholder="输入命令名称…"
          spellcheck="false"
          autocomplete="off"
        />
        <span class="palette-count">{{ enabledList.length }} 项</span>
      </div>

      <div ref="listEl" class="palette-list">
        <template v-for="(c, i) in filtered" :key="c.id">
          <div v-if="i === 0 || filtered[i - 1]?.group !== c.group" class="palette-group">{{ c.group }}</div>
          <button
            type="button"
            class="palette-item"
            :class="{ active: i === cursor, disabled: isDisabled(c) }"
            :data-cursor="i === cursor ? 'true' : undefined"
            :disabled="isDisabled(c)"
            @mouseenter="cursor = i"
            @click="runAt(i)"
          >
            <span class="pi-title" :title="c.title">{{ c.title }}</span>
            <span v-if="hintOf(c)" class="pi-hint" :title="hintOf(c)">{{ hintOf(c) }}</span>
          </button>
        </template>
        <div v-if="!filtered.length" class="palette-empty">没有匹配的命令</div>
      </div>

      <div class="modal-foot palette-foot">
        <span class="palette-keys"><kbd>↑</kbd><kbd>↓</kbd> 选择 · <kbd>Enter</kbd> 执行 · <kbd>Esc</kbd> 关闭</span>
      </div>
    </div>
  </div>
</template>

<style scoped>
.palette-modal {
  width: 520px;
  height: auto;
  max-height: calc(100vh - 80px);
  display: flex;
  flex-direction: column;
}
.sr-only {
  position: absolute;
  width: 1px;
  height: 1px;
  overflow: hidden;
  clip: rect(0 0 0 0);
}
.palette-head {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 14px 16px 10px;
  border-bottom: 1px solid var(--border);
}
.palette-input {
  flex: 1;
  border: none;
  background: transparent;
  color: var(--text);
  font-family: var(--mono);
  font-size: 14px;
  outline: none;
}
.palette-count {
  font-size: 11px;
  color: var(--muted-2);
  flex-shrink: 0;
}
.palette-list {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  padding: 6px 0 8px;
}
.palette-group {
  padding: 10px 16px 4px;
  font-size: 10px;
  letter-spacing: .4px;
  color: var(--muted-2);
  text-transform: uppercase;
  user-select: none;
}
.palette-item {
  display: flex;
  align-items: center;
  gap: 10px;
  width: calc(100% - 12px);
  margin: 0 6px;
  padding: 7px 10px;
  border: none;
  border-radius: 6px;
  background: transparent;
  color: var(--text);
  font-family: var(--mono);
  font-size: 12.5px;
  text-align: left;
  cursor: pointer;
}
.palette-item.active {
  background: var(--hover);
}
.palette-item.disabled {
  color: var(--muted-2);
  cursor: default;
}
.pi-title {
  flex: 1;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.pi-hint {
  font-size: 11px;
  color: var(--muted-2);
  /* 命令模板把命令原文放在这里回显：长命令不得把标题挤没，超宽省略号 + title 悬浮看全文 */
  flex-shrink: 0;
  max-width: 260px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.palette-empty {
  padding: 18px 16px;
  color: var(--muted-2);
  font-size: 12px;
  text-align: center;
}
.palette-foot {
  padding: 10px 16px 12px;
}
.palette-keys {
  color: var(--muted-2);
  font-size: 11px;
}
</style>
