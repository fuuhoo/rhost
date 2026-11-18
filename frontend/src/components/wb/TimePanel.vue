<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import { invoke } from '@tauri-apps/api/core'
import { toast } from '../../composables/useToast'

interface ServerTime {
  epochMs: number
  tzOffsetMinutes: number
  tzName: string
  formatted: string
}

interface NtpStatus {
  os: string
  distro: string
  service: string
  installed: boolean
  active: boolean
  enabled: boolean
  synchronized: boolean | null
  offsetMs: number | null
  servers: string[]
  configPath?: string
  packageManager: string
  timezone?: string
  timezones: string[]
  managed: boolean
  note?: string
  now: ServerTime
}

interface TimeChange {
  message: string
  status: NtpStatus
}

type RunElevated = <T>(fn: (password?: string) => Promise<T>) => Promise<T>

const props = defineProps<{ sessionId: string; runElevated: RunElevated }>()

const SERVICE_LABELS: Record<string, string> = {
  chrony: 'chrony',
  ntpd: 'ntpd',
  timesyncd: 'systemd-timesyncd',
  none: '未安装',
}

const status = ref<NtpStatus | null>(null)
const loading = ref(false)
const action = ref<'' | 'install' | 'configure' | 'sync' | 'service' | 'timezone'>('')
const error = ref('')
const serversText = ref('')
const serversDirty = ref(false)
const tzQuery = ref('')
const tzOpen = ref(false)

const COMMON_TIMEZONES = [
  'Asia/Shanghai', 'Asia/Hong_Kong', 'Asia/Taipei', 'Asia/Tokyo', 'Asia/Seoul',
  'Asia/Singapore', 'UTC', 'Europe/London', 'Europe/Berlin',
  'America/New_York', 'America/Los_Angeles',
]
const clockBase = ref<{ epoch: number; at: number } | null>(null)
const tick = ref(Date.now())

let timer: ReturnType<typeof setInterval> | null = null

const serviceLabel = computed(() =>
  status.value ? SERVICE_LABELS[status.value.service] ?? status.value.service : '—',
)

const tzLabel = computed(() => {
  const tz = status.value?.now
  if (!tz) return ''
  const total = tz.tzOffsetMinutes
  const sign = total < 0 ? '-' : '+'
  const abs = Math.abs(total)
  const hm = `UTC${sign}${String(Math.floor(abs / 60)).padStart(2, '0')}:${String(abs % 60).padStart(2, '0')}`
  const name = status.value?.timezone
  const zone = name && name !== tz.tzName ? `${name} · ` : ''
  return tz.tzName ? `${zone}${tz.tzName} · ${hm}` : `${zone}${hm}`
})

const tzSuggestions = computed(() => {
  const all = status.value?.timezones ?? []
  const q = tzQuery.value.trim().toLowerCase()
  if (!q) {
    return COMMON_TIMEZONES.filter(tz => !all.length || all.includes(tz))
  }
  return all.filter(tz => tz.toLowerCase().includes(q)).slice(0, 80)
})

function pickTimezone(tz: string) {
  tzQuery.value = tz
  tzOpen.value = false
}

let tzCloseTimer: ReturnType<typeof setTimeout> | null = null
function closeTzLater() {
  if (tzCloseTimer) clearTimeout(tzCloseTimer)
  tzCloseTimer = setTimeout(() => { tzOpen.value = false }, 150)
}
function openTzList() {
  if (tzCloseTimer) clearTimeout(tzCloseTimer)
  tzOpen.value = true
}

/** 以读取到的服务器时间为基准，本地每秒推进（网络往返误差不累积） */
const clockText = computed(() => {
  const base = clockBase.value
  if (!base) return '--:--:--'
  const ms = base.epoch + (tick.value - base.at)
  const shifted = new Date(ms + (status.value?.now.tzOffsetMinutes ?? 0) * 60_000)
  const p = (n: number) => String(n).padStart(2, '0')
  return `${shifted.getUTCFullYear()}-${p(shifted.getUTCMonth() + 1)}-${p(shifted.getUTCDate())} ` +
    `${p(shifted.getUTCHours())}:${p(shifted.getUTCMinutes())}:${p(shifted.getUTCSeconds())}`
})

const offsetLabel = computed(() => {
  const v = status.value?.offsetMs
  if (v === null || v === undefined) return ''
  const sign = v > 0 ? '+' : ''
  return `${sign}${v.toFixed(3)} ms`
})

const syncLabel = computed(() => {
  const s = status.value?.synchronized
  if (!status.value) return ''
  if (!status.value.installed) return '未安装时间服务'
  if (s === true) return '已与上游同步'
  if (s === false) return '尚未同步'
  return '同步状态未知'
})

const busyLabel = computed(() => {
  switch (action.value) {
    case 'install': return '正在安装 chrony，可能需要一两分钟…'
    case 'configure': return '正在写入时间源并重启服务…'
    case 'sync': return '正在立即校时…'
    case 'service': return '正在调整时间服务…'
    case 'timezone': return '正在写入时区…'
    default: return ''
  }
})

const canInstall = computed(() => !!status.value && status.value.managed && !status.value.installed)

function errText(e: unknown): string {
  if (typeof e === 'string') return e
  if (e instanceof Error) return e.message
  return String(e)
}

function applyStatus(next: NtpStatus) {
  status.value = next
  clockBase.value = { epoch: next.now.epochMs, at: Date.now() }
  if (!serversDirty.value) serversText.value = next.servers.join(' ')
}

async function load(silent = false) {
  const sid = props.sessionId
  if (!sid) return
  if (!silent) {
    loading.value = true
    error.value = ''
  }
  try {
    const res = await invoke<NtpStatus>('toolbox_ntp_status', { sessionId: sid })
    serversDirty.value = false
    applyStatus(res)
  } catch (e) {
    const msg = errText(e)
    if (msg !== '已取消输入 sudo 密码') error.value = msg
  } finally {
    if (!silent) loading.value = false
  }
}

async function runChange(kind: 'install' | 'configure' | 'sync' | 'service' | 'timezone',
  fn: (pw?: string) => Promise<TimeChange>) {
  action.value = kind
  error.value = ''
  try {
    const res = await props.runElevated(fn)
    serversDirty.value = false
    applyStatus(res.status)
    toast(res.message, 'ok')
  } catch (e) {
    const msg = errText(e)
    if (msg !== '已取消输入 sudo 密码') error.value = msg
  } finally {
    action.value = ''
  }
}

function install() {
  const sid = props.sessionId
  if (!sid) return
  void runChange('install', pw => invoke<TimeChange>('toolbox_ntp_install', {
    sessionId: sid,
    sudoPassword: pw,
  }))
}

function saveServers() {
  const sid = props.sessionId
  if (!sid) return
  const servers = serversText.value.split(/[\s,;]+/).map(s => s.trim()).filter(Boolean)
  if (!servers.length) {
    error.value = '至少填写一个 NTP 服务器'
    return
  }
  void runChange('configure', pw => invoke<TimeChange>('toolbox_ntp_configure', {
    sessionId: sid,
    servers,
    sudoPassword: pw,
  }))
}

function saveTimezone() {
  const sid = props.sessionId
  if (!sid) return
  const tz = tzQuery.value.trim()
  if (!tz) {
    error.value = '请填写时区，例如 Asia/Shanghai'
    return
  }
  tzOpen.value = false
  void runChange('timezone', pw => invoke<TimeChange>('toolbox_timezone_set', {
    sessionId: sid,
    timezone: tz,
    sudoPassword: pw,
  }))
}

function syncNow() {
  const sid = props.sessionId
  if (!sid) return
  void runChange('sync', pw => invoke<TimeChange>('toolbox_ntp_sync', {
    sessionId: sid,
    sudoPassword: pw,
  }))
}

function serviceAction(next: 'start' | 'stop' | 'enable' | 'disable') {
  const sid = props.sessionId
  if (!sid) return
  void runChange('service', pw => invoke<TimeChange>('toolbox_ntp_service', {
    sessionId: sid,
    action: next,
    sudoPassword: pw,
  }))
}

watch(() => props.sessionId, () => {
  status.value = null
  clockBase.value = null
  serversDirty.value = false
  serversText.value = ''
  tzQuery.value = ''
  tzOpen.value = false
  void load()
})

onMounted(() => {
  void load()
  timer = setInterval(() => { tick.value = Date.now() }, 1000)
})
onUnmounted(() => {
  if (timer) clearInterval(timer)
  if (tzCloseTimer) clearTimeout(tzCloseTimer)
})
</script>

<template>
  <div class="ts-root">
    <div v-if="error" class="ts-error">
      <span>{{ error }}</span>
      <button class="ts-link" @click="load()">重试</button>
    </div>

    <div class="ts-clock">
      <span class="ts-clock-main">{{ clockText }}</span>
      <span v-if="tzLabel" class="ts-clock-sub">{{ tzLabel }}</span>
      <span class="spacer"></span>
      <span v-if="loading" class="ts-clock-sub">正在读取…</span>
    </div>

    <section class="ts-card">
      <div class="ts-head">
        <h3>时间同步</h3>
        <span class="ts-pill" :class="{ on: status?.active }">{{ serviceLabel }}</span>
        <span v-if="status?.installed" class="ts-pill" :class="{ on: status.active }">
          {{ status.active ? '运行中' : '已停止' }}
        </span>
        <span v-if="status?.installed" class="ts-pill" :class="{ warn: status.synchronized === false }">
          {{ syncLabel }}
        </span>
        <span class="spacer"></span>
        <button class="ts-btn" :disabled="loading || !!action" @click="load()">刷新</button>
      </div>

      <div v-if="status" class="ts-meta">
        <span>{{ status.distro || status.os }}</span>
        <span v-if="status.configPath" :title="status.configPath">配置：{{ status.configPath }}</span>
        <span v-if="status.packageManager !== 'none'">包管理：{{ status.packageManager }}</span>
        <span v-if="status.offsetMs !== null && status.offsetMs !== undefined">偏差 {{ offsetLabel }}</span>
      </div>

      <div v-if="status?.servers.length" class="ts-servers">
        <span class="ts-label">时间源</span>
        <span v-for="s in status.servers" :key="s" class="ts-chip">{{ s }}</span>
      </div>

      <p v-if="status?.note" class="ts-note">{{ status.note }}</p>
      <p v-if="!status && !loading && !error" class="ts-empty">尚未读取</p>
    </section>

    <section v-if="status?.managed" class="ts-card">
      <div class="ts-head">
        <h3>时间源设置</h3>
      </div>
      <div class="ts-bar">
        <input
          v-model="serversText" class="ts-input"
          placeholder="例如 ntp.aliyun.com 或 10.0.0.1"
          autocomplete="off" spellcheck="false"
          :disabled="!status.installed || !!action"
          @input="serversDirty = true"
          @keydown.enter="saveServers"
        />
        <button class="ts-btn" :disabled="!status.installed || !!action" @click="saveServers">
          保存时间源
        </button>
      </div>
      <p class="ts-hint">
        多个服务器用空格或逗号分隔（最多 6 个）；保存会备份原配置为 <code>.rhost.bak</code> 并重启时间服务。
      </p>
    </section>

    <section v-if="status?.managed" class="ts-card">
      <div class="ts-head">
        <h3>时区设置</h3>
        <span v-if="status.timezone" class="ts-pill">{{ status.timezone }}</span>
        <span class="spacer"></span>
        <span v-if="action === 'timezone'" class="ts-busy">正在写入时区…</span>
      </div>
      <div class="ts-bar">
        <div class="ts-combo">
          <input
            v-model="tzQuery" class="ts-input"
            placeholder="搜索或输入时区，如 Asia/Shanghai"
            autocomplete="off" spellcheck="false"
            :disabled="!!action"
            @focus="openTzList"
            @input="openTzList"
            @blur="closeTzLater"
            @keydown.enter="saveTimezone"
            @keydown.esc="tzOpen = false"
          />
          <div v-if="tzOpen && tzSuggestions.length" class="ts-combo-list">
            <button
              v-for="tz in tzSuggestions" :key="tz"
              class="ts-combo-item" :class="{ active: tz === status.timezone }"
              @mousedown.prevent="pickTimezone(tz)"
            >{{ tz }}</button>
          </div>
        </div>
        <button class="ts-btn" :disabled="!!action || !tzQuery.trim()" @click="saveTimezone">
          设置时区
        </button>
      </div>
      <p class="ts-hint">
        时区只影响远端本地时间与日志时间显示，不改变时钟的绝对时间；NTP 对时仍按 UTC 进行。
      </p>
    </section>

    <section class="ts-card">
      <div class="ts-head">
        <h3>服务操作</h3>
        <span class="spacer"></span>
        <span v-if="busyLabel" class="ts-busy">{{ busyLabel }}</span>
      </div>
      <div class="ts-bar">
        <button v-if="canInstall" class="ts-btn primary" :disabled="!!action" @click="install">
          安装 chrony
        </button>
        <template v-else-if="status?.installed">
          <button class="ts-btn" :disabled="!!action" @click="syncNow">立即校时</button>
          <button v-if="status.active" class="ts-btn" :disabled="!!action" @click="serviceAction('stop')">
            停止服务
          </button>
          <button v-else class="ts-btn" :disabled="!!action" @click="serviceAction('start')">
            启动服务
          </button>
          <button v-if="status.enabled" class="ts-btn" :disabled="!!action" @click="serviceAction('disable')">
            取消开机自启
          </button>
          <button v-else class="ts-btn" :disabled="!!action" @click="serviceAction('enable')">
            开机自启
          </button>
        </template>
        <span v-else-if="status && !status.managed" class="ts-hint">当前平台不支持自动安装。</span>
      </div>
      <p v-if="status?.installed" class="ts-hint">
        立即校时会强制步进系统时间（chrony 为 <code>makestep</code>），请确认业务能接受时钟跳变。
      </p>
    </section>
  </div>
</template>

<style scoped>
.ts-root {
  flex: 1;
  min-height: 0;
  display: flex;
  flex-direction: column;
  gap: 10px;
  overflow: auto;
}
.ts-clock {
  display: flex;
  align-items: baseline;
  gap: 10px;
  padding: 12px 14px;
  border: 1px solid var(--border);
  border-radius: 9px;
  background: var(--panel-2);
  flex-shrink: 0;
}
.ts-clock-main {
  font-family: var(--mono, monospace);
  font-size: 21px;
  letter-spacing: .5px;
  color: var(--text);
  font-variant-numeric: tabular-nums;
}
.ts-clock-sub {
  font-size: 11px;
  color: var(--muted);
  white-space: nowrap;
}
.ts-card {
  border: 1px solid var(--border);
  border-radius: 9px;
  background: var(--panel-2);
  padding: 12px 14px;
  display: flex;
  flex-direction: column;
  gap: 9px;
  flex-shrink: 0;
}
.ts-head {
  display: flex;
  align-items: center;
  gap: 8px;
  min-width: 0;
}
.ts-head h3 {
  margin: 0;
  font-size: 12.5px;
  font-weight: 600;
  color: var(--text);
  white-space: nowrap;
}
.ts-pill {
  padding: 2px 8px;
  border-radius: 999px;
  border: 1px solid var(--border);
  font-size: 11px;
  color: var(--muted);
  white-space: nowrap;
}
.ts-pill.on {
  border-color: color-mix(in srgb, var(--green) 45%, transparent);
  color: var(--green);
}
.ts-pill.warn {
  border-color: color-mix(in srgb, var(--yellow) 50%, transparent);
  color: var(--yellow);
}
.ts-meta {
  display: flex;
  flex-wrap: wrap;
  gap: 6px 16px;
  font-size: 11px;
  color: var(--muted);
  min-width: 0;
}
.ts-meta span {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  max-width: 46em;
}
.ts-servers {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 6px;
}
.ts-label {
  font-size: 11px;
  color: var(--muted);
}
.ts-chip {
  padding: 2px 8px;
  border-radius: 6px;
  border: 1px solid var(--border);
  font-family: var(--mono, monospace);
  font-size: 11px;
  color: var(--text);
  word-break: break-all;
}
.ts-note {
  margin: 0;
  font-size: 11.5px;
  color: var(--muted);
  line-height: 1.6;
}
.ts-bar {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-wrap: wrap;
  min-width: 0;
}
.ts-input {
  flex: 1;
  min-width: 200px;
  padding: 6px 9px;
  border: 1px solid var(--border);
  border-radius: 7px;
  background: var(--panel-2);
  font: inherit;
  font-size: 11.5px;
  color: var(--text);
}
.ts-input:focus { outline: none; border-color: var(--border-strong); }
.ts-combo {
  position: relative;
  flex: 1;
  min-width: 220px;
}
.ts-combo .ts-input { width: 100%; box-sizing: border-box; }
.ts-combo-list {
  position: absolute;
  left: 0;
  right: 0;
  top: calc(100% + 4px);
  z-index: 20;
  max-height: 224px;
  overflow-y: auto;
  padding: 4px;
  border: 1px solid var(--border);
  border-radius: 8px;
  background: var(--panel);
  box-shadow: 0 10px 26px rgba(0, 0, 0, .22);
}
.ts-combo-item {
  display: block;
  width: 100%;
  padding: 5px 8px;
  border: 0;
  border-radius: 6px;
  background: none;
  font: inherit;
  font-size: 11.5px;
  color: var(--text);
  text-align: left;
  cursor: pointer;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.ts-combo-item:hover { background: var(--hover); }
.ts-combo-item.active { color: var(--blue); }
.ts-btn {
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
.ts-btn:hover:not(:disabled) { border-color: var(--border-strong); background: var(--hover); }
.ts-btn:disabled { opacity: .5; cursor: default; }
.ts-btn.primary {
  border-color: color-mix(in srgb, var(--blue) 55%, transparent);
  color: var(--blue);
}
.ts-busy {
  font-size: 11px;
  color: var(--muted);
}
.ts-hint {
  margin: 0;
  font-size: 11px;
  color: var(--muted-2);
  line-height: 1.6;
}
.ts-hint code {
  font-family: var(--mono, monospace);
  font-size: 10.5px;
}
.ts-empty {
  margin: 0;
  padding: 10px 0;
  text-align: center;
  font-size: 12px;
  color: var(--muted-2);
}
.ts-error {
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
.ts-link {
  border: 0;
  background: none;
  padding: 0;
  font: inherit;
  font-size: 11.5px;
  color: var(--blue);
  cursor: pointer;
  white-space: nowrap;
}
.spacer { flex: 1; }
</style>
