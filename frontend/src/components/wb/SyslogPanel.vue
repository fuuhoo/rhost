<script setup lang="ts">
import { computed, nextTick, onMounted, onUnmounted, ref, watch } from 'vue'
import { invoke } from '@tauri-apps/api/core'
import { save as saveDialog } from '@tauri-apps/plugin-dialog'
import { toast } from '../../composables/useToast'
import { copyText } from '../../stores/keys'

interface LogSource {
  id: string
  label: string
  kind: 'journal' | 'file' | 'macos'
  path?: string
  detail: string
  readable: boolean
}

interface SyslogInfo {
  os: string
  distro: string
  sources: LogSource[]
}

interface SyslogReadResult {
  output: string
  truncated: boolean
}

type RunElevated = <T>(fn: (password?: string) => Promise<T>) => Promise<T>

const props = defineProps<{ sessionId: string; runElevated: RunElevated }>()

type Level = 'error' | 'warn' | 'info' | 'debug' | 'other'
const RE_ERROR = /\b(error|err|critical|crit|fatal|fail|failed|failure|panic|emerg|alert)\b/i
const RE_WARN = /\b(warn|warning)\b/i
const RE_INFO = /\b(info|notice)\b/i
const RE_DEBUG = /\b(debug)\b/i

function classify(line: string): Level {
  if (RE_ERROR.test(line)) return 'error'
  if (RE_WARN.test(line)) return 'warn'
  if (RE_INFO.test(line)) return 'info'
  if (RE_DEBUG.test(line)) return 'debug'
  return 'other'
}

const MONTHS: Record<string, number> = {
  Jan: 0, Feb: 1, Mar: 2, Apr: 3, May: 4, Jun: 5,
  Jul: 6, Aug: 7, Sep: 8, Oct: 9, Nov: 10, Dec: 11,
}
// syslog / journald 短格式：Nov 11 10:50:13（无年份）
const RE_TIME_SHORT = /^([A-Z][a-z]{2})\s+(\d{1,2})\s+(\d{2}):(\d{2}):(\d{2})/
// ISO 格式（macOS log show 等）：2026-11-11 10:50:13
const RE_TIME_ISO = /^(\d{4})-(\d{2})-(\d{2})[ T](\d{2}):(\d{2}):(\d{2})/

/** 日志行首时间戳 → 可比较毫秒值（按主机本地墙钟，不做时区换算）。
 *  短格式无年份，按当前年份推断；若推断结果明显落在未来则回退一年（跨年日志）。 */
function parseLineTime(text: string): number | null {
  const iso = RE_TIME_ISO.exec(text)
  if (iso) return Date.UTC(+iso[1], +iso[2] - 1, +iso[3], +iso[4], +iso[5], +iso[6])
  const m = RE_TIME_SHORT.exec(text)
  if (!m) return null
  const mon = MONTHS[m[1]]
  if (mon === undefined) return null
  const now = new Date()
  let year = now.getFullYear()
  let ms = Date.UTC(year, mon, +m[2], +m[3], +m[4], +m[5])
  if (ms > now.getTime() + 2 * 86400000) {
    year -= 1
    ms = Date.UTC(year, mon, +m[2], +m[3], +m[4], +m[5])
  }
  return ms
}

/** 时间输入（YYYY-MM-DD HH:MM[:SS]，分隔符空格或 T）→ 毫秒；空串/格式不符返回 null。 */
function parseInputTime(v: string): number | null {
  const s = v.trim()
  if (!s) return null
  const m = /^(\d{4})-(\d{2})-(\d{2})[ T](\d{2}):(\d{2})(?::(\d{2}))?$/.exec(s)
  if (!m) return null
  return Date.UTC(+m[1], +m[2] - 1, +m[3], +m[4], +m[5], m[6] ? +m[6] : 0)
}

const TAIL_OPTIONS = [100, 200, 500, 1000, 2000, 5000]
const FOLLOW_INTERVAL_MS = 3000

const info = ref<SyslogInfo | null>(null)
const sourcesBusy = ref(false)
const selectedSource = ref('')
const tail = ref(200)
const result = ref<SyslogReadResult | null>(null)
const readBusy = ref(false)
const error = ref('')
const query = ref('')
const levelFilter = ref<'all' | Level>('all')
const since = ref('')
const until = ref('')
const sortDesc = ref(false)
const follow = ref(false)
const copied = ref(false)

const sources = computed(() => info.value?.sources ?? [])
const currentSource = computed(() => sources.value.find(s => s.id === selectedSource.value) ?? null)

const parsedLines = computed<{ text: string; level: Level; time: number | null }[]>(() => {
  const out = result.value?.output ?? ''
  if (!out) return []
  const arr = out.split('\n')
  if (arr.length && arr[arr.length - 1] === '') arr.pop()
  return arr.map(text => ({ text, level: classify(text), time: parseLineTime(text) }))
})

const sinceMs = computed(() => parseInputTime(since.value))
const untilMs = computed(() => {
  const ms = parseInputTime(until.value)
  if (ms === null) return null
  // 分钟精度（无秒）的结束时间含整分钟
  return until.value.trim().length <= 16 ? ms + 59_999 : ms
})
const timeActive = computed(() => sinceMs.value !== null || untilMs.value !== null)

const filteredLines = computed(() => {
  const q = query.value.trim().toLowerCase()
  const sm = sinceMs.value
  const um = untilMs.value
  return parsedLines.value.filter(l => {
    if (levelFilter.value !== 'all' && l.level !== levelFilter.value) return false
    if (sm !== null || um !== null) {
      if (l.time === null) return false
      if (sm !== null && l.time < sm) return false
      if (um !== null && l.time > um) return false
    }
    if (!q) return true
    return l.text.toLowerCase().includes(q)
  })
})

const displayLines = computed(() =>
  sortDesc.value ? [...filteredLines.value].reverse() : filteredLines.value,
)

const timeNote = computed(() => {
  if (!timeActive.value) return ''
  return currentSource.value?.kind === 'journal'
    ? 'journald 按时间段服务端查询'
    : '文件源仅在已加载的末尾行内按时间过滤'
})

const countLabel = computed(() => {
  if (!result.value) return ''
  const total = parsedLines.value.length
  const shown = filteredLines.value.length
  return shown === total ? `共 ${total} 行` : `命中 ${shown} / ${total} 行`
})

/* ---- 滚动跟随：仅当视图本就贴底时才自动滚到最新，避免把正在回看的历史拽走 ---- */
const logWrap = ref<HTMLElement | null>(null)
const atBottom = ref(true)
function onLogScroll() {
  const el = logWrap.value
  if (!el) return
  atBottom.value = el.scrollHeight - el.scrollTop - el.clientHeight < 40
}
function scrollBottom() {
  const el = logWrap.value
  if (el) el.scrollTop = el.scrollHeight
}

function errText(e: unknown): string {
  if (typeof e === 'string') return e
  if (e instanceof Error) return e.message
  return String(e)
}
function reportError(e: unknown) {
  const msg = errText(e)
  if (msg !== '已取消输入 sudo 密码') error.value = msg
}

async function readLog(silent = false) {
  const sid = props.sessionId
  if (!sid || !selectedSource.value) return
  if (!silent) {
    readBusy.value = true
    error.value = ''
  }
  try {
    const res = await props.runElevated(pw => invoke<SyslogReadResult>('toolbox_syslog_read', {
      sessionId: sid,
      source: selectedSource.value,
      tail: tail.value,
      since: since.value || null,
      until: until.value || null,
      sudoPassword: pw,
    }))
    result.value = res
    await nextTick()
    if (atBottom.value) scrollBottom()
  } catch (e) {
    if (silent) follow.value = false
    reportError(e)
  } finally {
    if (!silent) readBusy.value = false
  }
}

async function loadSources() {
  const sid = props.sessionId
  if (!sid) return
  sourcesBusy.value = true
  error.value = ''
  info.value = null
  result.value = null
  selectedSource.value = ''
  try {
    const res = await invoke<SyslogInfo>('toolbox_syslog_sources', { sessionId: sid })
    info.value = res
    if (res.sources.length) {
      selectedSource.value = res.sources[0].id
      atBottom.value = true
      await readLog()
    }
  } catch (e) {
    reportError(e)
  } finally {
    sourcesBusy.value = false
  }
}

function onSourceChange() {
  result.value = null
  atBottom.value = true
  void readLog()
}
function onTailChange() {
  atBottom.value = true
  void readLog()
}
function onTimeChange() {
  // journald 源把时间段交给服务端重新查询；文件源/macOS 由客户端过滤即时生效
  if (currentSource.value?.kind === 'journal') {
    atBottom.value = true
    void readLog()
  }
}
function clearTime() {
  since.value = ''
  until.value = ''
  onTimeChange()
}
function manualRefresh() {
  atBottom.value = true
  void readLog()
}
function retry() {
  if (!info.value) void loadSources()
  else void readLog()
}

/* ---- 实时跟随 ---- */
let followTimer: ReturnType<typeof setInterval> | null = null
function stopFollow() {
  if (followTimer) {
    clearInterval(followTimer)
    followTimer = null
  }
}
function startFollow() {
  stopFollow()
  followTimer = setInterval(() => {
    if (!props.sessionId || readBusy.value || sourcesBusy.value) return
    if (typeof document !== 'undefined' && document.visibilityState !== 'visible') return
    void readLog(true)
  }, FOLLOW_INTERVAL_MS)
}
watch(follow, v => {
  if (v) startFollow()
  else stopFollow()
})

async function copyLog() {
  const text = displayLines.value.map(l => l.text).join('\n')
  if (!text) return
  if (await copyText(text)) {
    copied.value = true
    setTimeout(() => { copied.value = false }, 1500)
  } else {
    toast('复制失败', 'err', 1600)
  }
}

function logFileName() {
  const d = new Date()
  const p = (n: number) => String(n).padStart(2, '0')
  const stamp = `${d.getFullYear()}${p(d.getMonth() + 1)}${p(d.getDate())}_${p(d.getHours())}${p(d.getMinutes())}${p(d.getSeconds())}`
  const src = currentSource.value
  const raw = src?.kind === 'file' && src.path
    ? (src.path.split('/').pop() || 'system')
    : (selectedSource.value || 'system')
  const base = raw.replace(/[^A-Za-z0-9._-]+/g, '_') || 'system'
  return `${base}_${stamp}.log`
}

async function downloadLog() {
  const text = displayLines.value.map(l => l.text).join('\n')
  if (!text) return
  const path = await saveDialog({
    title: '保存系统日志',
    defaultPath: logFileName(),
    filters: [{ name: '日志文件', extensions: ['log', 'txt'] }],
  })
  if (!path) return
  try {
    const abs = await invoke<string>('toolbox_syslog_save', { path, content: text })
    toast(`日志已保存：${abs}`, 'ok', 2800)
  } catch (e) {
    reportError(e)
  }
}

watch(() => props.sessionId, () => {
  follow.value = false
  stopFollow()
  query.value = ''
  levelFilter.value = 'all'
  since.value = ''
  until.value = ''
  sortDesc.value = false
  void loadSources()
})

onMounted(() => { void loadSources() })
onUnmounted(stopFollow)
</script>

<template>
  <div class="sl-root">
    <div class="sl-bar">
      <select
        v-model="selectedSource" class="sl-input sl-select sl-source"
        :disabled="sourcesBusy || !sources.length" title="日志来源"
        @change="onSourceChange"
      >
        <option v-for="s in sources" :key="s.id" :value="s.id">{{ s.label }}</option>
      </select>
      <span v-if="info" class="sl-distro" :title="`系统：${info.os}`">{{ info.distro }}</span>
      <select v-model.number="tail" class="sl-input sl-select" title="显示行数" @change="onTailChange">
        <option v-for="n in TAIL_OPTIONS" :key="n" :value="n">末尾 {{ n }} 行</option>
      </select>
      <label class="sl-check" title="每 3 秒自动刷新到最新">
        <input v-model="follow" type="checkbox" :disabled="!selectedSource" />
        实时
      </label>
      <span class="spacer"></span>
      <span v-if="result" class="sl-count">{{ countLabel }}</span>
      <button class="sl-btn" :disabled="readBusy || !selectedSource" @click="manualRefresh">刷新</button>
    </div>

    <div class="sl-bar">
      <input
        v-model="query" class="sl-input sl-search" placeholder="搜索日志内容…"
        autocomplete="off" spellcheck="false"
      />
      <select v-model="levelFilter" class="sl-input sl-select" title="级别筛选（按关键词归类）">
        <option value="all">全部级别</option>
        <option value="error">错误</option>
        <option value="warn">警告</option>
        <option value="info">信息</option>
        <option value="debug">调试</option>
        <option value="other">其他</option>
      </select>
      <span v-if="currentSource" class="sl-detail ellipsis" :title="currentSource.detail">
        {{ currentSource.detail }}
      </span>
      <span class="spacer"></span>
      <button class="sl-btn" :disabled="!filteredLines.length" @click="copyLog">
        {{ copied ? '已复制' : '复制' }}
      </button>
      <button class="sl-btn" :disabled="!filteredLines.length" @click="downloadLog">下载</button>
    </div>

    <div class="sl-bar">
      <span class="sl-time-label">时间</span>
      <input
        v-model="since" type="text" class="sl-input sl-time"
        placeholder="YYYY-MM-DD HH:MM" autocomplete="off" spellcheck="false"
        title="起始时间（含秒可写 YYYY-MM-DD HH:MM:SS）" @change="onTimeChange"
      />
      <span class="sl-time-sep">→</span>
      <input
        v-model="until" type="text" class="sl-input sl-time"
        placeholder="YYYY-MM-DD HH:MM" autocomplete="off" spellcheck="false"
        title="结束时间（含秒可写 YYYY-MM-DD HH:MM:SS）" @change="onTimeChange"
      />
      <button class="sl-btn" :disabled="!since && !until" @click="clearTime">清除</button>
      <select v-model="sortDesc" class="sl-input sl-select" title="排序方向">
        <option :value="false">最旧在前</option>
        <option :value="true">最新在前</option>
      </select>
      <span class="spacer"></span>
      <span v-if="timeNote" class="sl-time-note ellipsis" :title="timeNote">{{ timeNote }}</span>
    </div>

    <div v-if="error" class="sl-error">
      <span>{{ error }}</span>
      <button class="sl-link" @click="retry">重试</button>
    </div>
    <p v-if="result?.truncated" class="sl-hint">日志过长，仅显示末尾部分。</p>

    <div ref="logWrap" class="sl-wrap" @scroll="onLogScroll">
      <div v-if="sourcesBusy" class="sl-empty">正在探测日志来源…</div>
      <div v-else-if="!info" class="sl-empty">尚未读取</div>
      <div v-else-if="!sources.length" class="sl-empty">未检测到可用的系统日志来源</div>
      <div v-else-if="readBusy && !result" class="sl-empty">正在读取日志…</div>
      <div v-else-if="!parsedLines.length" class="sl-empty">（无输出）</div>
      <div v-else-if="!filteredLines.length" class="sl-empty">没有匹配筛选条件的日志</div>
      <template v-else>
        <div v-for="(l, i) in displayLines" :key="i" class="sl-line" :class="l.level">{{ l.text }}</div>
      </template>
    </div>
  </div>
</template>

<style scoped>
.sl-root {
  flex: 1;
  min-height: 0;
  display: flex;
  flex-direction: column;
  gap: 8px;
}
.sl-bar {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-shrink: 0;
  min-width: 0;
}
.sl-input {
  padding: 6px 9px;
  border: 1px solid var(--border);
  border-radius: 7px;
  background: var(--panel-2);
  font: inherit;
  font-size: 11.5px;
  color: var(--text);
  min-width: 0;
}
.sl-input:focus { outline: none; border-color: var(--border-strong); }
.sl-select { cursor: pointer; }
.sl-source { max-width: 280px; }
.sl-search { flex: 1; max-width: 320px; }
.sl-distro {
  font-size: 11px;
  color: var(--muted);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
  max-width: 180px;
}
.sl-detail {
  font-size: 11px;
  color: var(--muted-2);
  max-width: 240px;
}
.sl-time-label {
  font-size: 11px;
  color: var(--muted);
  white-space: nowrap;
}
.sl-time {
  font-family: var(--mono, monospace);
  font-size: 11px;
  width: 150px;
  flex-shrink: 0;
}
.sl-time-sep {
  font-size: 11px;
  color: var(--muted-2);
}
.sl-time-note {
  font-size: 11px;
  color: var(--muted-2);
  max-width: 300px;
}
.sl-check {
  display: flex;
  align-items: center;
  gap: 5px;
  font-size: 11px;
  color: var(--muted);
  white-space: nowrap;
  cursor: pointer;
}
.sl-count {
  font-size: 11px;
  color: var(--muted);
  white-space: nowrap;
}
.sl-btn {
  padding: 6px 12px;
  border: 1px solid var(--border);
  border-radius: 7px;
  background: var(--panel-2);
  font: inherit;
  font-size: 11.5px;
  color: var(--text);
  cursor: pointer;
  white-space: nowrap;
}
.sl-btn:hover:not(:disabled) { border-color: var(--border-strong); background: var(--hover); }
.sl-btn:disabled { opacity: .5; cursor: default; }
.sl-link {
  border: 0;
  background: none;
  padding: 0;
  font: inherit;
  font-size: 11.5px;
  color: var(--blue);
  cursor: pointer;
  white-space: nowrap;
}
.sl-link:hover { text-decoration: underline; }
.spacer { flex: 1; }
.ellipsis { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.sl-error {
  display: flex;
  align-items: flex-start;
  gap: 10px;
  padding: 8px 10px;
  border: 1px solid color-mix(in srgb, var(--red) 45%, transparent);
  border-radius: 8px;
  background: color-mix(in srgb, var(--red) 10%, transparent);
  font-size: 11.5px;
  color: var(--red);
  word-break: break-all;
  flex-shrink: 0;
}
.sl-hint {
  margin: 0;
  font-size: 11px;
  color: var(--muted);
  flex-shrink: 0;
}
.sl-empty {
  padding: 26px 0;
  text-align: center;
  font-size: 12px;
  color: var(--muted-2);
}
.sl-wrap {
  flex: 1;
  min-height: 0;
  border: 1px solid var(--border);
  border-radius: 8px;
  background: var(--panel-2);
  overflow: auto;
  padding: 8px 12px;
}
.sl-line {
  font-family: var(--mono, monospace);
  font-size: 11px;
  line-height: 1.55;
  color: var(--text);
  white-space: pre-wrap;
  word-break: break-all;
}
.sl-line.error { color: var(--red); }
.sl-line.warn { color: var(--yellow); }
.sl-line.debug { color: var(--muted-2); }
</style>