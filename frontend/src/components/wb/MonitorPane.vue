<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref, watch } from 'vue'
import { toast } from '../../composables/useToast'
import { copyText } from '../../stores/keys'
import {
  METRICS_HEARTBEAT_MS, activeHost, activeHostInfo, activeMetrics, activeNetHist,
  activeReconnectAttempt, activeSession, isExternalSession, monitorVisible,
  connected, disconnectSession, metricsHeartbeat, openDock, reconnectTick,
  startMetrics, stopMetrics,
} from '../../stores/session'
import { savedSettings } from '../../stores/settings'
import ProcModal from './ProcModal.vue'

/* ---------- 会话时长计时 ---------- */
const uptime = ref('00:00:00')
let timer: ReturnType<typeof setInterval> | null = null

const loginTime = computed(() => {
  const s = activeSession.value
  if (!s) return '—'
  return new Date(s.startedAt).toLocaleTimeString('zh-CN', { hour12: false })
})

function pad(n: number) {
  return String(n).padStart(2, '0')
}

onMounted(() => {
  timer = setInterval(() => {
    const s = activeSession.value
    if (s && s.state === 'online') {
      const t = Math.floor((Date.now() - s.startedAt) / 1000)
      uptime.value = `${pad(Math.floor(t / 3600))}:${pad(Math.floor(t / 60) % 60)}:${pad(t % 60)}`
    }
  }, 1000)
})
onUnmounted(() => {
  if (timer) clearInterval(timer)
  window.removeEventListener('mousemove', onPathMove)
  window.removeEventListener('mouseup', onPathEnd)
  window.clearTimeout(tipTimer)
})

/* ---------- 连接状态五态 ---------- */
const stateLabel = computed(() => {
  const s = activeSession.value
  if (!s) return '未连接'
  if (s.state === 'online') return '已连接'
  if (s.state === 'connecting') return '连接中…'
  if (s.state === 'reconnecting') {
    return activeReconnectAttempt.value > 0
      ? `重连中 · 第 ${activeReconnectAttempt.value} 次`
      : '重连中…'
  }
  if (s.state === 'idle') return '未连接'
  return '已断开'
})
const stateCls = computed(() => {
  const s = activeSession.value
  if (!s) return 'idle'
  if (s.state === 'online') return 'ok'
  if (s.state === 'connecting') return 'info'
  if (s.state === 'reconnecting') return 'warn'
  if (s.state === 'idle') return 'idle'
  return 'err'
})
/** 状态圆点对应的语义色变量 */
const stateDotVar = computed(() => {
  switch (stateCls.value) {
    case 'ok': return 'var(--rhost-status-success)'
    case 'info': return 'var(--rhost-status-processing)'
    case 'warn': return 'var(--rhost-status-warning)'
    case 'err': return 'var(--rhost-status-error)'
    default: return 'var(--rhost-status-idle)'
  }
})

/* ---------- 主机地址（带复制） ---------- */
const hostLabel = computed(() => activeSession.value?.host.id ?? '—')
/** 堡垒机拉起的外部连接：单通道，指标区整块不适用（见模板 v-if 分支） */
const externalSession = computed(() => isExternalSession(activeSession.value))
const hostIpPort = computed(() => {
  const s = activeSession.value
  return s ? `${s.host.ip}:${s.host.port}` : ''
})
/** 当前连接登录账号（本地配置，SSH 认证保证即实际账号，无需远端采集） */
const currentUser = computed(() => activeSession.value?.host.user || '—')
const authMethod = computed(() => {
  const s = activeSession.value
  if (!s) return '—'
  return s.host.keyPath ? '公钥认证' : '密码认证'
})
const latencyText = computed(() => {
  const lat = activeSession.value?.host.lat
  return lat ? `${lat} ms` : '—'
})

function copyAddr() {
  if (!hostIpPort.value) return
  navigator.clipboard?.writeText(hostIpPort.value).then(() => {
    toast('地址已复制到剪贴板', 'ok', 1400)
  }).catch(() => {
    toast('复制失败', 'err', 1200)
  })
}

async function copyProc(text: string, label: string) {
  const ok = await copyText(text)
  toast(ok ? `${label}已复制` : '复制失败', ok ? 'ok' : 'err', 1200)
}

/* ---------- 展开态与图表页签 ---------- */
/** 系统信息默认收起：头部只留状态/地址/账号，点「系统信息」再看发行版、内核等 */
const sysinfoOpen = ref(false)
/** 图表区页签：网络曲线（默认）/ CPU Top 进程 */
const chartTab = ref<'net' | 'proc'>('net')

/* ---------- 资源监控：CPU / 内存 / Swap ---------- */
/** 逻辑核数取真实 nproc（0x05），未到达前不显示具体数字 */
const cpuCoresText = computed(() =>
  activeHostInfo.value && activeHostInfo.value.cores > 0
    ? `${activeHostInfo.value.cores} 核`
    : '核数 N/A',
)
/** 当前在线会话的最新一帧真实指标；离线返回 null（进度条归零） */
const liveMetrics = computed(() => (connected.value ? activeMetrics.value : null))
const cpuPct = computed(() => liveMetrics.value?.cpu.util ?? 0)
/** 1/5/15 分钟负载文本（两位小数，运行队列长度非百分比）；未采集到不显示 */
const loadText = computed(() => {
  const l = liveMetrics.value?.cpu.load
  return l ? l.map(v => v.toFixed(2)).join(' / ') : null
})
const memPct = computed(() => {
  const x = liveMetrics.value?.mem
  if (!x || x.total <= 0) return 0
  return (x.used / x.total) * 100
})
/** 内存用量摘要：字节自适应单位；首帧未到/离线时占位（百分比画在条内，此处不重复） */
const memSummary = computed(() => {
  const x = liveMetrics.value?.mem
  if (!x || x.total <= 0) return '—'
  return `${fmtBytes(x.used)} / ${fmtBytes(x.total)}`
})
/** 无 swap（swap_total=0）时整行隐藏 Swap 横条，而非显示 0% */
const swapVisible = computed(() => (liveMetrics.value?.mem.swap_total ?? 0) > 0)
const swapPct = computed(() => {
  const x = liveMetrics.value?.mem
  if (!x || x.swap_total <= 0) return 0
  return (x.swap_used / x.swap_total) * 100
})
const swapSummary = computed(() => {
  const x = liveMetrics.value?.mem
  if (!x || x.swap_total <= 0) return '—'
  return `${fmtBytes(x.swap_used)} / ${fmtBytes(x.swap_total)}`
})

/** 字节自适应单位：<1GB 显示整数 MB，>=1GB 保留一位小数（≥10GB 取整） */
function fmtBytes(bytes: number) {
  const mb = bytes / (1024 * 1024)
  if (mb < 1024) return `${Math.round(mb)} MB`
  const gb = mb / 1024
  return `${gb >= 10 || Number.isInteger(gb) ? Math.round(gb) : gb.toFixed(1)} GB`
}

function meterClass(pct: number) {
  if (pct >= 85) return 'crit'
  if (pct >= 65) return 'warn'
  return ''
}

/* ---------- GPU：显存/温度格式化 ---------- */
/** 显存占用百分比（后端显存为字节口径） */
function gpuMemPct(g: { mem_used: number; mem_total: number }) {
  if (!g.mem_total) return 0
  return Math.min(100, (g.mem_used / g.mem_total) * 100)
}
/** 温度语义色：≥85°C 红、≥72°C 黄 */
function gpuTempClass(t: number) {
  if (t >= 85) return 'crit'
  if (t >= 72) return 'warn'
  return ''
}

/* ---------- 网络：真实速率（0x06）+ 按会话隔离的 sparkline 历史 ---------- */
/** 活动会话当前网络指标；离线/首帧未到为 null（速率显示 0） */
const liveNet = computed(() => (connected.value ? activeMetrics.value?.net ?? null : null))
const txRate = computed(() => liveNet.value?.tx_rate ?? 0)
const rxRate = computed(() => liveNet.value?.rx_rate ?? 0)

/** bytes/s 自适应速率文本：<1MB/s 用 KB（一位小数），否则 MB（两位小数） */
function fmtRate(bps: number) {
  if (bps < 1024 * 1024) return `${(bps / 1024).toFixed(1)} KB/s`
  return `${(bps / 1024 / 1024).toFixed(2)} MB/s`
}

/* ---------- 网络曲线图（页签区）：上传/下载双序列 ---------- */
const CHART_W = 100
const CHART_H = 46
/** 两条序列共用一个最大值定标：各自拉满会让量级差异看不出来 */
const netHistMax = computed(() => {
  const h = activeNetHist.value
  return Math.max(1, ...h.up, ...h.down)
})
/** 折线顶点（viewBox 坐标：0 落在图底、max 落在 y=4，静止时面积为 0 不成一条粗带） */
function chartLine(arr: number[], max: number) {
  return arr
    .map((v, i) => {
      const x = (i / (arr.length - 1)) * CHART_W
      const y = CHART_H - (v / max) * (CHART_H - 4)
      return `${x.toFixed(1)},${y.toFixed(1)}`
    })
    .join(' ')
}
/** 折线闭合到图底的面积多边形（比单线条更容易看出突发尖峰） */
function chartArea(arr: number[], max: number) {
  return `0,${CHART_H} ${chartLine(arr, max)} ${CHART_W},${CHART_H}`
}

/* ---------- 磁盘（0x06 Metrics.disks；df 低频夹带，非夹带轮沿用缓存） ---------- */
const liveDisks = computed(() => (connected.value ? activeMetrics.value?.disks ?? [] : []))

/* ---------- 磁盘表列宽拖拽：全列弹性（fr 比例），分隔线精确跟随鼠标 ----
 * 原理同文件列表：拖动只在分隔线两侧两列之间转移比例（A+B 总和不变）。 */
type DiskKey = 'path' | 'size'
const DISK_ORDER: DiskKey[] = ['path', 'size']
// 大小列要容下最长的「1014 MB / 2177 GB」且不能压到右侧使用率列，
// 窄栏下宁可让路径列先缩（路径本身可横向拖拽浏览）
const DISK_MIN: Record<DiskKey, number> = { path: 96, size: 112 }
const diskG = ref<Record<DiskKey, number>>({ path: 628, size: 115 })
let diskDrag: { strip: HTMLElement; a: DiskKey; b: DiskKey; sum: number; raf: number; targetG: number } | null = null

const diskTemplate = computed(
  () => `minmax(96px, ${diskG.value.path}fr) minmax(112px, ${diskG.value.size}fr) 34px`,
)

function startDiskResize(e: MouseEvent, a: DiskKey, b: DiskKey) {
  if (e.button !== 0) return
  e.preventDefault()
  e.stopPropagation()
  const strip = (e.currentTarget as HTMLElement).closest('.ri-tbl-head') as HTMLElement | null
  if (!strip) return
  const sum = diskG.value[a] + diskG.value[b]
  diskDrag = { strip, a, b, sum, raf: 0, targetG: diskG.value[a] }
  document.body.style.cursor = 'col-resize'
  document.body.style.userSelect = 'none'
  window.addEventListener('mousemove', onDiskMove)
  window.addEventListener('mouseup', onDiskEnd)
}

function onDiskMove(e: MouseEvent) {
  if (!diskDrag) return
  const { strip, a, b } = diskDrag
  const rect = strip.getBoundingClientRect()
  const aEl = strip.children[DISK_ORDER.indexOf(a)] as HTMLElement | undefined
  const bEl = strip.children[DISK_ORDER.indexOf(b)] as HTMLElement | undefined
  if (!aEl || !bEl) return
  const pairW = aEl.offsetWidth + bEl.offsetWidth
  const clamped = Math.min(pairW - DISK_MIN[b], Math.max(DISK_MIN[a], e.clientX - rect.left - aEl.offsetLeft))
  // 钳制直接以像素换算 grow（与条宽无关），避免条宽变化导致比例漂移、一拖变大
  diskDrag.targetG = diskDrag.sum * (clamped / pairW)
  if (diskDrag.raf) return
  diskDrag.raf = requestAnimationFrame(() => {
    if (diskDrag) {
      diskDrag.raf = 0
      const g = diskDrag.targetG
      diskG.value[diskDrag.a] = g
      diskG.value[diskDrag.b] = diskDrag.sum - g
    }
  })
}

function onDiskEnd() {
  window.removeEventListener('mousemove', onDiskMove)
  window.removeEventListener('mouseup', onDiskEnd)
  if (diskDrag?.raf) {
    cancelAnimationFrame(diskDrag.raf)
    diskDrag.raf = 0
  }
  document.body.style.cursor = ''
  document.body.style.userSelect = ''
  diskDrag = null
}

/* ---------- 磁盘路径：左右拖拽平移 + 悬浮全路径提示 + 点击复制 ----------
 * 路径列常被截断（docker overlay 路径冗长），横向滚不动就看不到全名：
 * 按住左右拖移浏览；悬停 250ms 弹自定义提示显示全部；单击（无拖动）复制路径。 */
let pdEl: HTMLElement | null = null
let pdStartX = 0
let pdStartLeft = 0
let pdMoved = false
let pdMount = ''
let pdRaf = 0
let pdTarget = 0

function onPathDown(e: MouseEvent, mount: string) {
  if (e.button !== 0) return
  e.preventDefault()
  const el = e.currentTarget as HTMLElement
  pdEl = el
  pdStartX = e.clientX
  pdStartLeft = el.scrollLeft
  pdMoved = false
  pdMount = mount
  window.addEventListener('mousemove', onPathMove)
  window.addEventListener('mouseup', onPathEnd)
}

function onPathMove(e: MouseEvent) {
  if (!pdEl) return
  const dx = e.clientX - pdStartX
  if (!pdMoved) {
    if (Math.abs(dx) <= 4) return
    pdMoved = true
    pdEl.classList.add('panning')
    onPathHoverEnd()
  }
  pdTarget = pdStartLeft - dx
  if (pdRaf) return
  pdRaf = requestAnimationFrame(() => {
    pdRaf = 0
    if (pdEl) pdEl.scrollLeft = pdTarget
  })
}

function onPathEnd() {
  window.removeEventListener('mousemove', onPathMove)
  window.removeEventListener('mouseup', onPathEnd)
  if (pdRaf) {
    cancelAnimationFrame(pdRaf)
    pdRaf = 0
  }
  const mount = pdMount
  pdEl?.classList.remove('panning')
  pdEl = null
  pdMount = ''
  if (!pdMoved && mount) {
    navigator.clipboard?.writeText(mount).then(() => {
      toast('路径已复制到剪贴板', 'ok', 1400)
    }).catch(() => {
      toast('复制失败', 'err', 1200)
    })
  }
}

/** 悬浮提示：内容与原来行 title 等价（全路径 + 文件系统 + 已用/总计），250ms 后出现 */
const pathTip = ref<{ x: number; y: number; text: string } | null>(null)
let tipTimer: number | undefined

function onPathHover(e: MouseEvent, d: { mount: string; fs?: string }) {
  window.clearTimeout(tipTimer)
  const text = `${d.mount}${d.fs ? ` (${d.fs})` : ''}`
  tipTimer = window.setTimeout(() => {
    pathTip.value = { x: e.clientX, y: e.clientY, text }
  }, 250)
}

function onPathHoverMove(e: MouseEvent) {
  if (pathTip.value) {
    pathTip.value.x = e.clientX
    pathTip.value.y = e.clientY
  }
}

function onPathHoverEnd() {
  window.clearTimeout(tipTimer)
  pathTip.value = null
}

/* ---------- 进程（0x06 Metrics.procs；Top 5 每轮随帧更新） ---------- */
const liveProcs = computed(() =>
  connected.value ? activeMetrics.value?.procs ?? null : null,
)
/** 页签标题上的进程总数 */
const procTotal = computed(() => liveProcs.value?.total ?? 0)
/** 进程弹窗：全量列表 + 排序，打开期间才向后端订阅全量 */
const procModal = ref(false)

/* ---------- GPU（0x06 Metrics.gpus；无 GPU 主机为空数组，区块整体隐藏） ---------- */
const liveGpus = computed(() =>
  connected.value ? activeMetrics.value?.gpus ?? [] : [],
)

/* ---------- 系统信息（0x05 HostInfo；未到达前发行版回退主机配置） ---------- */
const NA = 'N/A'
const sysinfo = computed(() => {
  const h = activeHostInfo.value
  return {
    distro: h?.distro || activeHost.value?.os || NA,
    kernel: h?.kernel || NA,
    arch: h?.arch || NA,
    cpuModel: h?.cpu_model || NA,
    uptime: h?.uptime || NA,
    timezone: h?.timezone || NA,
    ip: h?.ip && h.ip !== NA ? h.ip : NA,
  }
})

/** 图表区网卡标签：默认路由网卡名（0x05）；未采集到显示 N/A */
const netIface = computed(() => activeHostInfo.value?.iface || NA)

/* ---------- 自动刷新开关（默认开启） ---------- */
const autoRefresh = ref(true)
function toggleAutoRefresh() {
  autoRefresh.value = !autoRefresh.value
  toast(autoRefresh.value ? '已开启自动刷新' : '已关闭自动刷新', 'info', 1200)
}

/* ---------- 动态指标采集门控（0x06） ----------
 * 仅当「活动会话在线 + 左栏监控展开 + 面板在视口 + 页面可见 + 自动刷新」
 * 全部满足时才让后端采集；任一条件翻转即停，后端 9s 心跳 TTL 兜底。 */
const rootEl = ref<HTMLElement | null>(null)
const panelVisible = ref(true)
const docVisible = ref(typeof document === 'undefined' ? true : document.visibilityState === 'visible')

const metricsTarget = computed<{ id: string; iface: string | null; intervalMs: number } | null>(() => {
  const s = activeSession.value
  if (!s || s.state !== 'online' || !s.backendId) return null
  // 外部连接（堡垒机链接）只有一条业务通道：指标采集要另开 exec 子通道，
  // 后端也会直接拒绝（EXTERNAL_SINGLE_CHANNEL），这里不发无谓的请求
  if (isExternalSession(s)) return null
  if (!autoRefresh.value || !monitorVisible.value || !panelVisible.value || !docVisible.value) {
    return null
  }
  // iface 取自 0x05：首帧若晚到，target 变化会触发一次幂等重启切到默认网卡
  // intervalMs 取自全局设置：改采集间隔时 target 变化，幂等重启 collector 即时生效
  const intervalSec = Math.min(10, Math.max(1, Math.round(savedSettings.metricsInterval) || 2))
  return { id: s.backendId, iface: activeHostInfo.value?.iface || null, intervalMs: intervalSec * 1000 }
})

let runningBackendId: string | null = null
let runningIface: string | null = null
let runningIntervalMs: number | null = null
let hbTimer: ReturnType<typeof setInterval> | null = null

/** 按目标（后端会话 id + 网卡 + 采集间隔）做状态机式收敛：相同目标不重复启停 */
function reconcileMetrics(target: { id: string; iface: string | null; intervalMs: number } | null) {
  if (
    target
    && runningBackendId === target.id
    && runningIface === target.iface
    && runningIntervalMs === target.intervalMs
  ) return
  if (runningBackendId) {
    void stopMetrics(runningBackendId)
    runningBackendId = null
    runningIface = null
    runningIntervalMs = null
  }
  if (hbTimer) {
    clearInterval(hbTimer)
    hbTimer = null
  }
  if (target) {
    void startMetrics(target.id, target.iface, target.intervalMs)
    runningBackendId = target.id
    runningIface = target.iface
    runningIntervalMs = target.intervalMs
    hbTimer = setInterval(() => {
      if (runningBackendId) metricsHeartbeat(runningBackendId)
    }, METRICS_HEARTBEAT_MS)
  }
}

watch(metricsTarget, target => reconcileMetrics(target), { immediate: true })

function onDocVisibility() {
  docVisible.value = document.visibilityState === 'visible'
}
let panelObserver: IntersectionObserver | null = null
onMounted(() => {
  document.addEventListener('visibilitychange', onDocVisibility)
  // 面板以 v-show 常驻：display:none 时 isIntersecting 为 false
  if (rootEl.value && typeof IntersectionObserver !== 'undefined') {
    panelObserver = new IntersectionObserver(entries => {
      panelVisible.value = entries[0]?.isIntersecting ?? true
    })
    panelObserver.observe(rootEl.value)
  }
})
onUnmounted(() => {
  reconcileMetrics(null)
  document.removeEventListener('visibilitychange', onDocVisibility)
  panelObserver?.disconnect()
})

/* ---------- 快捷操作（保留现有） ---------- */
function onReconnect() {
  if (!activeSession.value) {
    toast('没有活动会话', 'warn')
    return
  }
  reconnectTick.value++
}
async function onDisconnect() {
  const s = activeSession.value
  // 在线断开 SSH；重连等待中则取消自动重连（同一入口）
  if (!s || (s.state !== 'online' && s.state !== 'reconnecting')) {
    toast('当前没有活动会话', 'warn')
    return
  }
  const wasWaiting = s.state === 'reconnecting'
  if (await disconnectSession(s.id)) {
    toast(wasWaiting ? '已取消自动重连并断开' : '已断开连接', 'info', 1600)
  }
}
</script>

<template>
  <aside ref="rootEl" class="ri-root">
    <!-- 操作按钮注入左栏 MonitorPanel 头部。
         defer 必需：目标与本组件同在尚未插入 document 的子树里，挂载瞬间取不到 target
         会让 teleport 内容不挂载（vnode.el 为 null），下次更新即在 patchElement 崩溃。 -->
    <Teleport defer to="#mp-head-actions">
      <button class="ri-icon-btn" title="重新连接" @click="onReconnect">
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor"
             stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <path d="M21 12a9 9 0 1 1-2.64-6.36" />
          <path d="M21 3v6h-6" />
        </svg>
      </button>
      <button class="ri-icon-btn ri-toggle" :class="{ on: autoRefresh }"
              title="自动刷新"
              @click="toggleAutoRefresh">
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <path d="M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8"/>
          <path d="M21 3v5h-5"/>
          <path d="M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16"/>
          <path d="M8 16H3v5"/>
        </svg>
      </button>
      <button class="ri-icon-btn" title="打开 SFTP 文件管理"
              @click="openDock(); toast('已打开 SFTP 文件管理', 'info', 1600)">
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor"
             stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z" />
        </svg>
      </button>
      <button class="ri-icon-btn danger" title="断开连接" @click="onDisconnect">
        <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <path d="M18.36 6.64a9 9 0 1 1-12.73 0"/>
          <line x1="12" y1="2" x2="12" y2="12"/>
        </svg>
      </button>
    </Teleport>

    <!-- 滚动内容 -->
    <div class="ri-body">
      <!-- ① 连接头部：状态 / 主机 / 地址 / 账号 / 系统信息（折叠） -->
      <div class="ri-head">
        <div class="ri-head-row">
          <span class="ri-state" :class="`st-${stateCls}`">
            <span class="ri-pulse" :style="{ background: stateDotVar }" />
            {{ stateLabel }}
          </span>
          <span class="ri-uptime mono" title="本次会话时长">{{ uptime }}</span>
        </div>
        <div class="ri-hostname" :title="hostLabel">{{ hostLabel }}</div>
        <div class="ri-addr">
          <span class="k">IP</span>
          <span class="v mono">{{ hostIpPort || '—' }}</span>
          <button v-if="hostIpPort" class="ri-copy" title="复制地址" @click="copyAddr">复制</button>
        </div>
        <div class="ri-meta">
          <span class="mono">{{ currentUser }}</span>
          <span class="sep">·</span>
          <span>{{ authMethod }}</span>
          <span class="sep">·</span>
          <span class="mono">RTT {{ latencyText }}</span>
        </div>
        <button class="ri-sys-btn" :class="{ on: sysinfoOpen }" @click="sysinfoOpen = !sysinfoOpen">
          <span>系统信息</span>
          <svg class="chev" width="11" height="11" viewBox="0 0 24 24" fill="none"
               stroke="currentColor" stroke-width="2.4" stroke-linecap="round"
               stroke-linejoin="round">
            <path d="M6 9l6 6 6-6" />
          </svg>
        </button>
        <div v-if="sysinfoOpen" class="ri-sys">
          <div class="ri-kv"><span class="k">发行版</span><span class="v" :title="sysinfo.distro">{{ sysinfo.distro }}</span></div>
          <div class="ri-kv"><span class="k">内核</span><span class="v mono" :title="sysinfo.kernel">{{ sysinfo.kernel }}</span></div>
          <div class="ri-kv"><span class="k">架构</span><span class="v mono">{{ sysinfo.arch }}</span></div>
          <div class="ri-kv"><span class="k">CPU 型号</span><span class="v mono" :title="sysinfo.cpuModel">{{ sysinfo.cpuModel }}</span></div>
          <div class="ri-kv"><span class="k">时区</span><span class="v mono">{{ sysinfo.timezone }}</span></div>
          <div class="ri-kv"><span class="k">服务器 IP</span><span class="v mono" :title="sysinfo.ip">{{ sysinfo.ip }}</span></div>
          <div class="ri-kv"><span class="k">登录时间</span><span class="v mono">{{ loginTime }}</span></div>
        </div>
      </div>

      <!-- 外部连接（堡垒机链接）不开放指标采集通道：整块指标区以一行说明替代 -->
      <div v-if="externalSession" class="ri-empty">
        外部连接 · 单通道：不采集主机指标（CPU / 内存 / 网络 / 进程 / 磁盘）
      </div>

      <template v-else>
      <!-- ② 主机运行时长 + 负载：纯文本行，放在资源条上方 -->
      <div class="ri-texts">
        <div class="ri-text"><span class="k">运行</span><span class="v mono">{{ sysinfo.uptime }}</span></div>
        <div class="ri-text"><span class="k">负载</span><span class="v mono">{{ loadText ?? NA }}</span></div>
      </div>

      <!-- ③ 资源横条：百分比画在条内左侧，用量数字右对齐（FinalShell 口径） -->
      <div class="ri-bars">
        <div class="ri-bar">
          <span class="lb">CPU</span>
          <div class="track">
            <i class="fill cpu" :class="meterClass(cpuPct)" :style="{ width: cpuPct + '%' }" />
            <span class="pct">{{ cpuPct.toFixed(0) }}%</span>
            <span class="aux">{{ cpuCoresText }}</span>
          </div>
        </div>

        <div class="ri-bar">
          <span class="lb">内存</span>
          <div class="track">
            <i class="fill" :class="meterClass(memPct)" :style="{ width: memPct + '%' }" />
            <span class="pct">{{ memPct.toFixed(0) }}%</span>
            <span class="aux">{{ memSummary }}</span>
          </div>
        </div>

        <!-- 无 swap 的主机（swap_total=0）整行隐藏 -->
        <div v-if="swapVisible" class="ri-bar">
          <span class="lb">交换</span>
          <div class="track">
            <i class="fill swap" :style="{ width: swapPct + '%' }" />
            <span class="pct">{{ swapPct.toFixed(0) }}%</span>
            <span class="aux">{{ swapSummary }}</span>
          </div>
        </div>
      </div>

      <!-- ④ GPU：与 CPU 同款横条；无 GPU 主机整块隐藏 -->
      <div v-if="liveGpus.length" class="ri-gpus">
        <div v-for="g in liveGpus" :key="g.index" class="ri-gpu">
          <div class="ri-gpu-name" :title="g.name">
            <em class="ri-gpu-tag">GPU{{ g.index }}</em>
            <span class="ri-gpu-model">{{ g.name }}</span>
          </div>
          <div class="ri-bar">
            <span class="lb">利用率</span>
            <div class="track">
              <i class="fill cpu" :class="meterClass(g.util)" :style="{ width: g.util + '%' }" />
              <span class="pct">{{ Math.round(g.util) }}%</span>
              <span class="aux">
                <span :class="gpuTempClass(g.temp)">{{ Math.round(g.temp) }}°C</span>
                <template v-if="g.power_limit > 0"> · {{ Math.round(g.power) }}/{{ g.power_limit }}W</template>
              </span>
            </div>
          </div>
          <div class="ri-bar">
            <span class="lb">显存</span>
            <div class="track">
              <i class="fill vram" :style="{ width: gpuMemPct(g) + '%' }" />
              <span class="pct">{{ gpuMemPct(g).toFixed(0) }}%</span>
              <span class="aux">{{ fmtBytes(g.mem_used) }} / {{ fmtBytes(g.mem_total) }}</span>
            </div>
          </div>
        </div>
      </div>

      <!-- ⑤ 图表区：网络速率曲线 / CPU Top 进程 两页签 -->
      <div class="ri-tabs">
        <button class="ri-tab" :class="{ on: chartTab === 'net' }" @click="chartTab = 'net'">网络</button>
        <button class="ri-tab" :class="{ on: chartTab === 'proc' }" @click="chartTab = 'proc'">
          进程 · {{ procTotal }}
        </button>
        <button
          v-if="chartTab === 'proc'" class="ri-proc-expand" title="展开全部进程（可排序）"
          @click="procModal = true"
        >
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
               stroke-linecap="round" stroke-linejoin="round">
            <path d="M14 4h6v6" /><path d="M20 4l-7 7" />
            <path d="M10 20H4v-6" /><path d="M4 20l7-7" />
          </svg>
        </button>
      </div>

      <div v-if="chartTab === 'net'" class="ri-chart">
        <div class="ri-chart-top">
          <span class="ri-rate up">▲ {{ fmtRate(txRate) }}</span>
          <span class="ri-rate down">▼ {{ fmtRate(rxRate) }}</span>
          <span class="ri-iface" title="采样网卡（默认路由）">{{ netIface }}</span>
        </div>
        <div class="ri-plot">
          <span class="tick hi">{{ fmtRate(netHistMax) }}</span>
          <span class="tick lo">0</span>
          <svg :viewBox="`0 0 ${CHART_W} ${CHART_H}`" preserveAspectRatio="none">
            <line class="grid" x1="0" y1="4" :x2="CHART_W" y2="4" />
            <line class="grid" x1="0" y1="18" :x2="CHART_W" y2="18" />
            <line class="grid" x1="0" y1="32" :x2="CHART_W" y2="32" />
            <polygon class="area-up" :points="chartArea(activeNetHist.up, netHistMax)" />
            <polyline class="line-up" :points="chartLine(activeNetHist.up, netHistMax)" />
            <polygon class="area-dn" :points="chartArea(activeNetHist.down, netHistMax)" />
            <polyline class="line-dn" :points="chartLine(activeNetHist.down, netHistMax)" />
          </svg>
        </div>
      </div>
      <div v-else class="ri-proc">
        <div class="ri-proc-head">
          <span>进程</span><span>CPU</span><span>内存</span>
        </div>
        <div v-if="!liveProcs || liveProcs.top.length === 0" class="ri-empty">等待采集…</div>
        <div v-for="p in liveProcs?.top ?? []" :key="p.pid" class="ri-proc-row">
          <span class="pname">
            <span class="pc-name" :title="p.name">{{ p.name }}</span>
            <button
              type="button" class="pc-pid" :title="`点击复制进程 ID：${p.pid}`"
              @click.stop="copyProc(String(p.pid), '进程 ID')"
            >#{{ p.pid }}</button>
            <button
              v-if="p.container" type="button" class="cname"
              :title="`点击复制容器名称：${p.container.replace(/^\//, '')}`"
              @click.stop="copyProc(p.container.replace(/^\//, ''), '容器名称')"
            >{{ p.container.replace(/^\//, '') }}</button>
          </span>
          <span class="num" :class="{ 'cpu-hi': p.cpu >= 10, 'cpu-crit': p.cpu >= 20 }">
            {{ p.cpu.toFixed(1) }}
          </span>
          <span class="num mem">{{ fmtBytes(p.rss) }}</span>
        </div>
      </div>

      <!-- ⑥ 磁盘：路径 | 可用/大小 | 使用率 密集表格（挂载点多时内部滚动） -->
      <div class="ri-block" :style="{ '--dt-cols': diskTemplate }">
        <div class="ri-block-head">
          <span>磁盘</span>
          <span class="count">{{ liveDisks.length ? `${liveDisks.length} 个挂载点` : NA }}</span>
        </div>
        <div class="ri-tbl-head">
          <span>路径<i class="ri-grip" title="拖动调整列宽" @mousedown="startDiskResize($event, 'path', 'size')" @click.stop="() => {}"></i></span><span>可用 / 大小</span><span>使用</span>
        </div>
        <div v-if="liveDisks.length === 0" class="ri-empty">等待采集…</div>
        <div v-else class="ri-tbl-body">
          <div
            v-for="d in liveDisks"
            :key="d.mount"
            class="ri-tbl-row"
          >
            <span
              class="m"
              @mousedown="onPathDown($event, d.mount)"
              @mouseenter="onPathHover($event, d)"
              @mousemove="onPathHoverMove"
              @mouseleave="onPathHoverEnd"
            >
              <span class="mp">{{ d.mount }}</span>
              <i v-if="d.fs" class="fst">{{ d.fs }}</i>
            </span>
            <span class="s"><i class="sbar" :style="{ width: (d.total > 0 ? (d.available / d.total) * 100 : 0) + '%' }" /><span class="st">{{ fmtBytes(d.available) }} / {{ fmtBytes(d.total) }}</span></span>
            <span class="u" :class="meterClass(d.pct)">{{ d.pct }}%</span>
          </div>
        </div>
      </div>
      </template>
    </div>

    <!-- 磁盘路径悬浮提示：固定定位跟随光标，250ms 后出现 -->
    <div
      v-if="pathTip"
      class="ri-pathtip"
      :style="{ left: `${pathTip.x + 12}px`, top: `${pathTip.y + 16}px` }"
    >{{ pathTip.text }}</div>

    <!-- 进程弹窗：全量进程 + CPU/内存/容器排序，打开期间才订阅全量 -->
    <ProcModal v-model:visible="procModal" />
  </aside>
</template>

<style scoped>
/* 根容器：作为左栏 MonitorPanel 的内容填充；宽度/背景/边框由外层提供 */
.ri-root {
  display: flex;
  flex-direction: column;
  width: 100%;
  height: 100%;
  padding: 0;
  overflow: hidden;
}

/* ============ 头部（注入左栏面板头部的操作按钮） ============ */
.ri-icon-btn {
  width: 28px;
  height: 28px;
  border-radius: 6px;
  display: grid;
  place-items: center;
  background: transparent;
  border: 1px solid transparent;
  color: var(--muted-2);
  cursor: pointer;
  transition: background 0.15s, color 0.15s, border-color 0.15s;
}
.ri-icon-btn:hover {
  background: var(--panel-2);
  border-color: var(--border);
  color: var(--text);
}
.ri-icon-btn:active {
  transform: scale(0.94);
}
.ri-icon-btn.danger:hover {
  background: rgba(247, 118, 142, 0.12);
  border-color: rgba(247, 118, 142, 0.4);
  color: var(--red);
}

/* 自动刷新开关：开启=品牌绿+持续旋转，关闭=与普通按钮一致 */
.ri-toggle.on {
  color: var(--green);
}
.ri-toggle.on svg {
  transform-origin: 50% 50%;
  animation: ri-spin 3s linear infinite;
}
@keyframes ri-spin {
  to {
    transform: rotate(360deg);
  }
}
/* ============ 滚动内容 ============ */
.ri-body {
  flex: 1;
  overflow-y: auto;
  overflow-x: hidden;
  padding: 0 12px 12px;
  /* 专注模式下面板很宽：内容统一限宽 720px 居中；
     用 flex + align-items 居中，避免被子元素自身 margin 覆盖而失效 */
  display: flex;
  flex-direction: column;
  align-items: center;
}
.ri-body > * {
  width: 100%;
  max-width: 400px;
  box-sizing: border-box;
  flex-shrink: 0;
}
.ri-body::-webkit-scrollbar {
  width: 8px;
}
.ri-body::-webkit-scrollbar-thumb {
  background: var(--scrollbar-thumb);
  border-radius: 4px;
  border: 2px solid transparent;
  background-clip: padding-box;
}
.ri-body::-webkit-scrollbar-thumb:hover {
  background: var(--scrollbar-thumb-hover);
  background-clip: padding-box;
}

/* ============ ① 连接头部 ============ */
.ri-body .mono {
  font-family: var(--mono);
}
.ri-head {
  padding: 12px 0 11px;
  border-bottom: 1px solid var(--border-soft);
}
.ri-head-row {
  display: flex;
  align-items: center;
  gap: 8px;
}
.ri-state {
  display: flex;
  align-items: center;
  gap: 6px;
  font-size: 12px;
  font-weight: 600;
}
.ri-state.st-ok { color: var(--rhost-status-success); }
.ri-state.st-info { color: var(--rhost-status-processing); }
.ri-state.st-warn { color: var(--rhost-status-warning); }
.ri-state.st-err { color: var(--rhost-status-error); }
.ri-state.st-idle { color: var(--rhost-status-idle); }
.ri-uptime {
  margin-left: auto;
  font-size: 11.5px;
  color: var(--muted);
}
.ri-pulse {
  width: 7px;
  height: 7px;
  border-radius: 50%;
  flex: 0 0 auto;
  box-shadow: 0 0 0 0 currentColor;
  animation: ri-pulse 2s infinite;
}
@keyframes ri-pulse {
  0% {
    box-shadow: 0 0 0 0 rgba(61, 220, 132, 0.5);
  }
  70% {
    box-shadow: 0 0 0 7px rgba(61, 220, 132, 0);
  }
  100% {
    box-shadow: 0 0 0 0 rgba(61, 220, 132, 0);
  }
}
.ri-hostname {
  margin-top: 7px;
  font-size: 14px;
  font-weight: 600;
  color: var(--text-strong);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.ri-addr {
  margin-top: 4px;
  display: flex;
  align-items: center;
  gap: 6px;
  font-size: 11.5px;
  min-width: 0;
}
.ri-addr .k {
  flex: none;
  font-size: 10px;
  font-weight: 700;
  letter-spacing: 0.08em;
  color: var(--muted-2);
}
.ri-addr .v {
  min-width: 0;
  color: var(--text-soft);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.ri-copy {
  margin-left: auto;
  flex: none;
  padding: 1px 5px;
  border: 0;
  border-radius: 4px;
  background: none;
  color: var(--muted);
  font-size: 11px;
  cursor: pointer;
  transition: background 0.15s, color 0.15s;
}
.ri-copy:hover {
  color: var(--green);
  background: rgba(61, 220, 132, 0.12);
}
.ri-meta {
  margin-top: 3px;
  display: flex;
  align-items: center;
  gap: 5px;
  font-size: 11px;
  color: var(--muted);
  white-space: nowrap;
  overflow: hidden;
}
.ri-meta .sep {
  color: var(--fainter);
}

/* 系统信息：默认收起，整宽按钮点开（FinalShell 的「系统信息」同位） */
.ri-sys-btn {
  margin-top: 10px;
  width: 100%;
  height: 26px;
  display: flex;
  align-items: center;
  justify-content: center;
  gap: 5px;
  border: 1px solid var(--border);
  border-radius: 6px;
  background: var(--panel-2);
  color: var(--text-soft);
  font-size: 11.5px;
  cursor: pointer;
  transition: background 0.15s, border-color 0.15s, color 0.15s;
}
.ri-sys-btn:hover {
  border-color: var(--border-strong);
  color: var(--text);
}
.ri-sys-btn.on {
  background: var(--hover);
  color: var(--text);
}
.ri-sys-btn .chev {
  transition: transform 0.18s ease;
  opacity: 0.65;
}
.ri-sys-btn.on .chev {
  transform: rotate(180deg);
}
.ri-sys {
  margin-top: 8px;
  padding: 7px 9px;
  border: 1px solid var(--border-soft);
  border-radius: 7px;
  background: var(--veil-weak);
}

/* ============ ② 运行 / 负载文本行 ============ */
.ri-texts {
  padding: 10px 0 0;
  display: flex;
  flex-direction: column;
  gap: 3px;
}
.ri-text {
  display: flex;
  align-items: baseline;
  gap: 7px;
  font-size: 11.5px;
  color: var(--muted);
}
.ri-text .v {
  color: var(--text);
}

/* 键值行 */
.ri-kv {
  display: flex;
  align-items: baseline;
  gap: 10px;
  padding: 3.5px 0;
  font-size: 12px;
}
.ri-kv .k {
  flex: 0 0 auto;
  color: var(--muted);
}
.ri-kv .v {
  margin-left: auto;
  text-align: right;
  color: var(--text);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  min-width: 0;
  max-width: 66%;
}
.ri-kv .v.mono {
  font-family: var(--mono);
  font-size: 11.5px;
}

/* ============ ③ 资源横条：百分比在条内左、用量在条内右 ============ */
.ri-bars {
  padding: 10px 0 0;
  display: flex;
  flex-direction: column;
  gap: 6px;
}
.ri-bar {
  display: flex;
  align-items: center;
  gap: 8px;
}
.ri-bar .lb {
  flex: 0 0 40px;
  font-size: 11px;
  color: var(--muted);
}
.ri-bar .track {
  position: relative;
  flex: 1 1 auto;
  min-width: 0;
  height: 20px;
  box-sizing: border-box;
  display: flex;
  align-items: center;
  gap: 6px;
  padding: 0 6px;
  border: 1px solid var(--border);
  border-radius: 4px;
  background: var(--panel-deep);
  overflow: hidden;
}
/* 填充半透明压在文字下面：深浅两套主题里条内文字都保持原对比度；
   截图（FinalShell）口径：CPU 绿、内存/交换桃色、显存桃色 */
.ri-bar .fill {
  position: absolute;
  left: 0;
  top: 0;
  bottom: 0;
  border-radius: 0 3px 3px 0;
  background: #f6b07e;
  opacity: 0.3;
  transition: width 0.5s ease, background 0.3s;
}
.ri-bar .fill.cpu { background: #4fbf67; }
.ri-bar .fill.warn { background: var(--yellow); }
.ri-bar .fill.crit { background: var(--red); }
.ri-bar .fill.swap { background: #f6b07e; }
.ri-bar .fill.vram { background: #f6b07e; }
.ri-bar .pct,
.ri-bar .aux {
  position: relative;
  z-index: 1;
  font-family: var(--mono);
  font-size: 10.5px;
  white-space: nowrap;
}
.ri-bar .pct {
  color: var(--text);
}
.ri-bar .aux {
  margin-left: auto;
  color: var(--text-soft);
}
/* 条内文字压在半透明填充上：语义色向强文本靠拢，避免浅底浅字对比不足 */
.ri-bar .aux .warn {
  color: color-mix(in srgb, var(--yellow) 55%, var(--text-strong));
}
.ri-bar .aux .crit {
  color: color-mix(in srgb, var(--red) 55%, var(--text-strong));
}

/* ============ ④ GPU（与 CPU 同款横条） ============ */
.ri-gpus {
  padding: 12px 0 0;
}
.ri-gpu + .ri-gpu {
  margin-top: 11px;
  padding-top: 11px;
  border-top: 1px solid var(--border-soft);
}
.ri-gpu-name {
  display: flex;
  align-items: center;
  gap: 7px;
  margin-bottom: 7px;
  min-width: 0;
}
.ri-gpu-tag {
  flex: 0 0 auto;
  font-style: normal;
  font-family: var(--mono);
  font-size: 9.5px;
  font-weight: 700;
  letter-spacing: 0.04em;
  color: var(--cyan);
  background: rgba(79, 214, 224, 0.1);
  border: 1px solid rgba(79, 214, 224, 0.25);
  border-radius: 4px;
  padding: 1px 5px;
}
.ri-gpu-model {
  font-family: var(--mono);
  font-size: 11.5px;
  color: var(--text);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.ri-gpus .ri-bar + .ri-bar {
  margin-top: 5px;
}
/* GPU 的利用率/显存标签比 CPU 长一档 */
.ri-gpus .ri-bar .lb {
  flex: 0 0 44px;
}

/* ============ ⑤ 页签区：网络曲线 / Top 进程 ============ */
.ri-tabs {
  margin-top: 15px;
  display: flex;
  border: 1px solid var(--border);
  border-radius: 7px;
  overflow: hidden;
  background: var(--panel-deep);
}
.ri-tab {
  flex: 1 1 0;
  min-width: 0;
  height: 27px;
  border: 0;
  background: transparent;
  color: var(--muted);
  font-size: 11.5px;
  cursor: pointer;
  transition: background 0.15s, color 0.15s;
}
.ri-tab + .ri-tab {
  border-left: 1px solid var(--border);
}
.ri-tab:hover {
  color: var(--text);
  background: var(--veil-weak);
}
.ri-tab.on {
  background: var(--panel-2);
  color: var(--text-strong);
  font-weight: 600;
  box-shadow: inset 0 -2px 0 var(--green);
}
/* 进程页签右侧的展开按钮：打开全量进程弹窗 */
.ri-proc-expand {
  flex: 0 0 30px;
  display: flex;
  align-items: center;
  justify-content: center;
  border: 0;
  border-left: 1px solid var(--border);
  background: transparent;
  color: var(--muted);
  cursor: pointer;
  transition: background 0.15s, color 0.15s;
}
.ri-proc-expand:hover {
  color: var(--text);
  background: var(--veil-weak);
}
.ri-proc-expand svg {
  width: 13px;
  height: 13px;
}
.ri-chart {
  margin-top: 8px;
  padding: 8px 9px 7px;
  border: 1px solid var(--border-soft);
  border-radius: 8px;
  background: var(--panel-card);
}
.ri-chart-top {
  display: flex;
  align-items: center;
  gap: 10px;
  font-family: var(--mono);
  font-size: 11px;
}
.ri-rate.up {
  color: #e89a3c;
}
.ri-rate.down {
  color: #4ade80;
}
.ri-iface {
  margin-left: auto;
  color: var(--muted);
  font-size: 10.5px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
/* 纵轴刻度用 HTML 定位：SVG 的 preserveAspectRatio=none 会把 <text> 横向拉变形 */
.ri-plot {
  position: relative;
  margin-top: 5px;
  padding-left: 48px;
  height: 88px;
}
.ri-plot svg {
  display: block;
  width: 100%;
  height: 100%;
}
.ri-plot .tick {
  position: absolute;
  left: 0;
  font-family: var(--mono);
  font-size: 9.5px;
  color: var(--muted-2);
  white-space: nowrap;
}
.ri-plot .tick.hi {
  top: 0;
}
.ri-plot .tick.lo {
  bottom: 0;
}
.ri-plot .grid {
  stroke: var(--border-strong);
  stroke-width: 1;
  stroke-dasharray: 2 3;
  vector-effect: non-scaling-stroke;
}
.ri-plot .line-up {
  fill: none;
  stroke: #e89a3c;
  stroke-width: 1.5;
  stroke-linejoin: round;
  vector-effect: non-scaling-stroke;
}
.ri-plot .line-dn {
  fill: none;
  stroke: #4ade80;
  stroke-width: 1.5;
  stroke-linejoin: round;
  vector-effect: non-scaling-stroke;
}
.ri-plot .area-up {
  fill: #e89a3c;
  opacity: 0.15;
  stroke: none;
}
.ri-plot .area-dn {
  fill: #4ade80;
  opacity: 0.15;
  stroke: none;
}

/* ============ ⑥ 磁盘表格：路径 | 可用/大小 | 使用率 ============ */
.ri-block {
  margin-top: 15px;
  padding-top: 11px;
  border-top: 1px solid var(--border-soft);
  /* 尾部区块：纵向占满面板剩余高度，表体内部滚动——全屏不再留白 */
  flex: 1 1 auto;
  min-height: 0;
  display: flex;
  flex-direction: column;
}
.ri-block-head {
  display: flex;
  align-items: baseline;
  gap: 8px;
  margin-bottom: 6px;
  font-size: 10.5px;
  font-weight: 700;
  letter-spacing: 0.08em;
  color: var(--muted-2);
}
.ri-block-head .count {
  margin-left: auto;
  font-weight: 400;
  letter-spacing: 0;
  font-family: var(--mono);
  font-size: 10.5px;
}
.ri-tbl-head,
.ri-tbl-row {
  display: grid;
  grid-template-columns: var(--dt-cols, minmax(110px, 1fr) auto 34px);
  gap: 8px;
  align-items: center;
  padding: 0 5px;
}
/* 列宽拖拽把手：挂在表头列右缘，悬停显强调色 */
.ri-tbl-head span {
  position: relative;
}
.ri-grip {
  /* 正中压在表头右边缘的竖线上 */
  position: absolute;
  top: -4px;
  bottom: -4px;
  right: -5px;
  width: 10px;
  z-index: 2;
  cursor: col-resize;
  touch-action: none;
}
.ri-grip:hover {
  background: rgba(var(--accent-rgb), 0.45);
}
.ri-grip:hover::before {
  content: '';
  position: absolute;
  inset: 0 4px 0 5px;
  border-right: 2px solid rgba(var(--accent-rgb), 0.9);
}
.ri-tbl-head {
  padding-top: 4px;
  padding-bottom: 4px;
  background: var(--panel-2);
  font-size: 10.5px;
  font-weight: 600;
  color: var(--text);
}
.ri-tbl-head span:not(:first-child),
.ri-tbl-row span:not(:first-child) {
  text-align: right;
  font-family: var(--mono);
}
/* 竖线分隔符（FinalShell 表格口径）：只加在表头列头上，内容行不加 */
.ri-tbl-head span:not(:last-child) {
  border-right: 1px solid var(--border-soft);
}
html[data-theme="light"] .ri-tbl-head span:not(:last-child) {
  border-right-color: #d9dee6;
}
/* 挂载点可能几十条（每个容器一条 overlay），表体固定为剩余高度内滚动 */
.ri-tbl-body {
  flex: 1 1 auto;
  min-height: 0;
  overflow-y: auto;
  overscroll-behavior: contain;
}
.ri-tbl-body::-webkit-scrollbar {
  width: 8px;
}
.ri-tbl-body::-webkit-scrollbar-thumb {
  background: var(--scrollbar-thumb);
  border-radius: 4px;
  border: 2px solid transparent;
  background-clip: padding-box;
}
.ri-tbl-row {
  padding-top: 4px;
  padding-bottom: 4px;
  font-size: 11px;
}
.ri-tbl-row:nth-child(odd) {
  background: var(--veil-weak);
}
.ri-tbl-row .m {
  display: flex;
  align-items: center;
  min-width: 0;
  font-family: var(--mono);
  color: var(--text);
  /* 长路径可横移浏览（grab 拖拽），滚动条隐藏 */
  overflow-x: auto;
  overflow-y: hidden;
  scrollbar-width: none;
  white-space: nowrap;
  cursor: grab;
  user-select: none;
  touch-action: pan-y;
}
.ri-tbl-row .m::-webkit-scrollbar {
  display: none;
}
.ri-tbl-row .m.panning {
  cursor: grabbing;
}
/* 路径不截断省略：超出部分由 .m 横移呈现；fs 徽标跟随在路径后 */
.ri-tbl-row .m .mp {
  flex: 0 0 auto;
}
/* 磁盘路径悬浮提示：固定定位跟随光标，pointer-events 不挡悬停/拖拽 */
.ri-pathtip {
  position: fixed;
  z-index: 9999;
  pointer-events: none;
  max-width: 70vw;
  padding: 4px 8px;
  border-radius: 6px;
  background: var(--menu-bg);
  border: 1px solid var(--border);
  color: var(--text);
  font-family: var(--mono);
  font-size: 11.5px;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
  box-shadow: 0 6px 20px rgba(0, 0, 0, 0.35);
}
.ri-tbl-row .m .fst {
  flex-shrink: 0;
  margin-left: 5px;
  padding: 0 4px;
  border-radius: 3px;
  background: var(--fill-soft, rgba(128, 128, 128, 0.14));
  font-size: 9px;
  font-style: normal;
  line-height: 14px;
  color: var(--muted);
}
.ri-tbl-row .s {
  position: relative;
  min-width: 0;
  overflow: hidden;
  color: var(--text-soft);
  white-space: nowrap;
}
/* 数字层压在图条之上：条只做比例衬底，不遮字；超出列宽时整列裁切，
 * 绝不溢出到右侧使用率列造成重叠 */
.ri-tbl-row .s .st {
  position: relative;
  z-index: 1;
  padding: 0 2px;
}
/* 薄荷绿条：衬底显示「可用/总计」比例（截图口径），数字盖在条上 */
.ri-tbl-row .sbar {
  position: absolute;
  left: 0;
  top: 1px;
  bottom: 1px;
  border-radius: 3px;
  background: rgba(94, 200, 160, 0.2);
}
.ri-tbl-row .u {
  color: var(--muted);
}
.ri-tbl-row .u.warn {
  color: var(--yellow);
}
.ri-tbl-row .u.crit {
  color: var(--red);
}
.ri-empty {
  padding: 6px 5px;
  font-size: 11.5px;
  color: var(--muted);
}

/* ============ 进程页签 ============ */
.ri-proc {
  margin-top: 8px;
  padding: 6px 4px 7px;
  border: 1px solid var(--border-soft);
  border-radius: 8px;
  background: var(--panel-card);
}
.ri-proc-head,
.ri-proc-row {
  display: grid;
  grid-template-columns: minmax(0, 1fr) 44px 56px;
  gap: 8px;
  align-items: center;
  padding: 0 5px;
  font-size: 11.5px;
}
.ri-proc-head {
  background: #3e7fc0;
  color: #fff;
  font-size: 11px;
  font-weight: 600;
  padding-top: 5px;
  padding-bottom: 5px;
  border-radius: 3px 3px 0 0;
}
.ri-proc-row {
  padding-top: 4px;
  padding-bottom: 4px;
}
/* 截图斑马纹：偶数行浅蓝灰，深浅主题均可见 */
.ri-proc-row:nth-child(even) {
  background: rgba(94, 156, 214, 0.12);
}
.ri-proc-head span:not(:first-child),
.ri-proc-row span:not(:first-child) {
  text-align: right;
  font-family: var(--mono);
}
.ri-proc-row .pname {
  display: flex;
  align-items: center;
  min-width: 0;
  font-family: var(--mono);
  color: var(--text);
  overflow: hidden;
  white-space: nowrap;
}
.ri-proc-row .pc-name {
  flex: 0 0 auto;
  max-width: 55%;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.ri-proc-row .pc-pid,
.ri-proc-row .pname .cname {
  border: 0;
  background: transparent;
  font: inherit;
  cursor: copy;
}
.ri-proc-row .pc-pid {
  flex: 0 0 auto;
  margin-left: 5px;
  padding: 0 2px;
  border-radius: 3px;
  color: var(--muted-2);
}
.ri-proc-row .pc-pid:hover {
  background: var(--panel-2);
  color: var(--text);
  text-decoration: underline dotted;
}
/* 容器归属徽标：窄栏时优先截断容器名，不能把进程名挤没 */
.ri-proc-row .pname .cname {
  flex: 0 1 auto;
  min-width: 0;
  max-width: 45%;
  margin-left: 5px;
  padding: 0 5px;
  border-radius: 8px;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  background: rgba(94, 156, 214, 0.16);
  color: #5b9bd5;
  font-size: 9.5px;
  line-height: 14px;
}
.ri-proc-row .pname .cname:hover { text-decoration: underline dotted; }
.ri-proc-row .pc-pid:focus-visible,
.ri-proc-row .pname .cname:focus-visible {
  outline: 1px solid var(--blue);
  outline-offset: 1px;
}
html[data-theme="light"] .ri-proc-row .pname .cname {
  background: #eaf3fb;
  color: #3e7fc0;
}
.ri-proc-row .num.cpu-hi {
  color: var(--yellow);
}
.ri-proc-row .num.cpu-crit {
  color: var(--red);
}
.ri-proc-row .num.mem {
  color: var(--muted);
}

</style>
