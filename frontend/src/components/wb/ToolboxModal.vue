<script setup lang="ts">
import { computed, onMounted, onUnmounted, reactive, ref, watch } from 'vue'
import { Channel, invoke } from '@tauri-apps/api/core'
import { showToolbox } from '../../lib/uiBus'
import { cachedSudo, forgetSudo, rememberSudo } from '../../lib/toolboxSudo'
import { activeHost, activeSession } from '../../stores/session'
import { passwordPromptState, promptPassword } from '../../composables/usePasswordPrompt'
import { confirmDialog, confirmState } from '../../composables/useConfirm'
import { open as openDialog, save as saveDialog } from '@tauri-apps/plugin-dialog'
import { toast } from '../../composables/useToast'
import { copyText, keys } from '../../stores/keys'
import type { SshKey } from '../../types'
import DockerTerminal, { type DockerTermChannel } from './DockerTerminal.vue'
import SyslogPanel from './SyslogPanel.vue'
import TimePanel from './TimePanel.vue'

interface PortRow {
  port: number
  protocol: string
  address: string
  pid?: number
  process?: string
  user?: string
  container?: string
}

interface PortsResult {
  tool: string
  elevated: boolean
  rows: PortRow[]
}

interface FirewallRule {
  action: string
  port?: string
  protocol?: string
  source?: string
  origin: string
  raw: string
  deletable: boolean
}

interface FirewallStatus {
  backend: string
  enabled: boolean
  rules: FirewallRule[]
  note?: string
}

interface ToggleResult {
  status: FirewallStatus
  sshPortAutoOpened: boolean
}

interface PubkeyDeployResult {
  path: string
  /** false 表示该公钥已存在，未重复追加 */
  added: boolean
}

interface DockerContainer {
  id: string
  name: string
  image: string
  state: string
  status: string
  ports: string
  created: string
}

interface DockerImage {
  id: string
  repository: string
  tag: string
  /** 相对时间（如 `2 weeks ago`），仅展示 */
  created: string
  /** 原始时间戳，供按创建时间排序 */
  createdAt: string
  size: string
}

interface DockerNetwork {
  id: string
  name: string
  driver: string
  scope: string
  subnet: string
  gateway: string
  internal: boolean
  ipv6: boolean
  containers: string[]
}

interface DockerVolume {
  name: string
  driver: string
  scope: string
  /** 宿主机上的数据目录；部分旧版 daemon 为空 */
  mountpoint: string
  /** 创建时间；部分 daemon 不返回该字段 */
  createdAt: string
}

interface DockerListResult {
  elevated: boolean
  dockerVersion: string | null
  composeVersion: string | null
  containers: DockerContainer[]
  images: DockerImage[]
  networks: DockerNetwork[]
  volumes: DockerVolume[]
}

interface DockerLogsResult {
  output: string
  truncated: boolean
}

interface DockerConfig {
  version: string
  daemonJson: string | null
  configPath: string
  registryMirrors: string[]
  /** daemon.json 里的 data-root；null 表示使用 Docker 默认目录 */
  dataRoot: string | null
}

type Tab = 'firewall' | 'ports' | 'pubkey' | 'docker' | 'syslog' | 'time'
type DockerView = 'containers' | 'images' | 'networks' | 'volumes' | 'config'

const SUDO_REQUIRED = 'SUDO_PASSWORD_REQUIRED'
const SUDO_INCORRECT = 'SUDO_PASSWORD_INCORRECT'

const tab = ref<Tab>('docker')
const busy = ref<'' | 'fw' | 'fw-toggle' | 'ports' | 'add' | 'remove' | 'pubkey' | 'docker' | 'docker-act'>('')
const error = ref('')

const backendId = computed(() => activeSession.value?.backendId)
const sessionReady = computed(() => !!backendId.value)
const sessionLabel = computed(
  () => activeHost.value?.label || activeHost.value?.ip || '当前会话',
)
const sshPort = computed(() => activeHost.value?.port || 22)

/* ---- 防火墙 ---- */
const firewall = ref<FirewallStatus | null>(null)
const form = reactive({ action: 'allow', port: '', protocol: 'tcp', source: '' })

const firewallRows = computed(() => firewall.value?.rules ?? [])

const fwQuery = ref('')
const fwActionFilter = ref<'all' | 'allow' | 'deny' | 'other'>('all')
const showAddForm = ref(false)

const fwBackendLabel = computed(() => {
  const b = firewall.value?.backend
  if (!b) return '探测中…'
  if (b === 'none') return '未检测到防火墙'
  return b.toUpperCase()
})

const canToggleFirewall = computed(() => {
  const b = firewall.value?.backend
  return b === 'ufw' || b === 'firewalld'
})

const fwStats = computed(() => {
  const rows = firewallRows.value
  const allow = rows.filter(r => r.action === 'allow').length
  const deny = rows.filter(r => r.action === 'deny').length
  return { total: rows.length, allow, deny, other: rows.length - allow - deny }
})

const filteredFirewallRows = computed(() => {
  const q = fwQuery.value.trim().toLowerCase()
  return firewallRows.value.filter(r => {
    if (fwActionFilter.value !== 'all' && r.action !== fwActionFilter.value) return false
    if (!q) return true
    return (
      (r.port ?? '').toLowerCase().includes(q) ||
      (r.protocol ?? '').toLowerCase().includes(q) ||
      (r.source ?? '').toLowerCase().includes(q) ||
      r.raw.toLowerCase().includes(q)
    )
  })
})

/* ---- 端口 ---- */
const ports = ref<PortsResult | null>(null)
const portQuery = ref('')
const protoFilter = ref<'all' | 'tcp' | 'udp'>('all')

const filteredPorts = computed(() => {
  const q = portQuery.value.trim().toLowerCase()
  return (ports.value?.rows ?? []).filter(r => {
    if (protoFilter.value !== 'all' && r.protocol !== protoFilter.value) return false
    if (!q) return true
    return (
      String(r.port).includes(q) ||
      r.address.toLowerCase().includes(q) ||
      (r.process ?? '').toLowerCase().includes(q) ||
      (r.user ?? '').toLowerCase().includes(q) ||
      (r.container ?? '').toLowerCase().includes(q)
    )
  })
})

const portsCountLabel = computed(() => {
  const total = ports.value?.rows.length ?? 0
  const shown = filteredPorts.value.length
  return shown === total ? `共 ${total} 个监听端口` : `共 ${shown} / ${total} 个监听端口`
})

async function copyContainer(name?: string) {
  if (!name) return
  const ok = await copyText(name)
  toast(ok ? `已复制容器名：${name}` : '复制失败', ok ? 'info' : 'err', 1600)
}

/* ---- 公钥 ---- */
const keySource = ref<string>('manual')
const manualKey = ref('')
const targetUser = ref(activeHost.value?.user ?? '')
const pubkeyResult = ref<PubkeyDeployResult | null>(null)

const selectedKey = computed(() => keys.value.find(k => k.id === keySource.value) ?? null)

/** 后端 publicKey 可能已带注释（直接读 .pub 文件），避免重复拼接 */
function composePublicKey(k: SshKey): string {
  const base = k.publicKey.trim()
  if (base.split(/\s+/).length > 2) return base
  const comment = k.comment.trim()
  return comment ? `${base} ${comment}` : base
}

const keyLine = computed(() =>
  selectedKey.value ? composePublicKey(selectedKey.value) : manualKey.value.trim(),
)
const deployUser = computed(() => targetUser.value.trim())
const pubkeyPathPreview = computed(() =>
  deployUser.value ? `~${deployUser.value}/.ssh/authorized_keys` : '~/.ssh/authorized_keys',
)

function resetPubkeyForm() {
  targetUser.value = activeHost.value?.user ?? ''
  manualKey.value = ''
  pubkeyResult.value = null
  keySource.value = keys.value[0]?.id ?? 'manual'
}

async function deployPubkey() {
  const sid = backendId.value
  if (!sid) return
  if (!deployUser.value) {
    error.value = '请填写目标用户'
    return
  }
  if (!keyLine.value) {
    error.value = '请选择或粘贴公钥'
    return
  }
  busy.value = 'pubkey'
  error.value = ''
  pubkeyResult.value = null
  try {
    const res = await runElevated(pw => invoke<PubkeyDeployResult>('toolbox_deploy_pubkey', {
      sessionId: sid,
      publicKey: keyLine.value,
      targetUser: deployUser.value,
      sudoPassword: pw,
    }))
    pubkeyResult.value = res
    toast(
      res.added ? `公钥已写入 ${deployUser.value}` : `${deployUser.value} 已存在该公钥`,
      'ok',
      2400,
    )
  } catch (e) {
    reportError(e)
  } finally {
    busy.value = ''
  }
}

/* ---- Docker ---- */
const docker = ref<DockerListResult | null>(null)
const dockerView = ref<DockerView>('containers')
const dockerQuery = ref('')
const dockerStateFilter = ref<'all' | 'running' | 'exited' | 'paused' | 'other'>('all')

const dockerContainers = computed(() => docker.value?.containers ?? [])
const dockerImages = computed(() => docker.value?.images ?? [])
const dockerNetworks = computed(() => docker.value?.networks ?? [])
const dockerVolumes = computed(() => docker.value?.volumes ?? [])
const netCreate = ref<{ name: string, driver: string, subnet: string, gateway: string } | null>(null)
const volCreate = ref<{ name: string, driver: string } | null>(null)

/** 悬空镜像没有标签，用镜像 ID 作为删除目标 */
function imageRef(img: DockerImage): string {
  return img.tag === '<none>' ? img.id : `${img.repository}:${img.tag}`
}

function shortId(id: string): string {
  return id.replace(/^sha256:/, '').slice(0, 12)
}

const DOCKER_STATES: Record<string, string> = {
  running: '运行中',
  exited: '已停止',
  paused: '已暂停',
  created: '已创建',
  restarting: '重启中',
  removing: '删除中',
  dead: '已失效',
}

function dockerStateLabel(state: string): string {
  return DOCKER_STATES[state] ?? state
}

const filteredContainers = computed(() => {
  const q = dockerQuery.value.trim().toLowerCase()
  return dockerContainers.value.filter(c => {
    if (dockerStateFilter.value !== 'all') {
      if (dockerStateFilter.value === 'other') {
        if (['running', 'exited', 'paused'].includes(c.state)) return false
      } else if (c.state !== dockerStateFilter.value) {
        return false
      }
    }
    if (!q) return true
    return [c.name, c.image, dockerStateLabel(c.state), c.state, c.status, c.ports, c.id].some(v =>
      v.toLowerCase().includes(q),
    )
  })
})

const filteredImages = computed(() => {
  const q = dockerQuery.value.trim().toLowerCase()
  return dockerImages.value.filter(img => {
    if (!q) return true
    return [imageRef(img), img.repository, img.tag, img.id, img.size].some(v =>
      v.toLowerCase().includes(q),
    )
  })
})

const filteredNetworks = computed(() => {
  const q = dockerQuery.value.trim().toLowerCase()
  return dockerNetworks.value.filter(n => {
    if (!q) return true
    return [n.name, n.driver, n.scope, n.subnet, n.gateway, n.id, ...n.containers].some(v =>
      v.toLowerCase().includes(q),
    )
  })
})

const filteredVolumes = computed(() => {
  const q = dockerQuery.value.trim().toLowerCase()
  return dockerVolumes.value.filter(v => {
    if (!q) return true
    return [v.name, v.driver, v.scope, v.mountpoint, v.createdAt].some(field =>
      field.toLowerCase().includes(q),
    )
  })
})

/* ---- Docker 列表：表头排序与拖动列宽 ---- */
type DockerTable = 'containers' | 'images' | 'networks' | 'volumes'

interface DockerSortState { key: string, dir: 'asc' | 'desc' }
const dockerSort = reactive<Record<DockerTable, DockerSortState>>({
  containers: { key: '', dir: 'asc' },
  images: { key: '', dir: 'asc' },
  networks: { key: '', dir: 'asc' },
  volumes: { key: '', dir: 'asc' },
})

const DOCKER_COLUMNS: Record<DockerTable, { key: string, label: string }[]> = {
  containers: [
    { key: 'name', label: '名称' },
    { key: 'image', label: '镜像' },
    { key: 'state', label: '状态' },
    { key: 'ports', label: '端口' },
    { key: 'created', label: '创建时间' },
    { key: '', label: '操作' },
  ],
  images: [
    { key: 'repo', label: '仓库:标签' },
    { key: 'id', label: '镜像 ID' },
    { key: 'size', label: '大小' },
    { key: 'created', label: '创建时间' },
    { key: '', label: '操作' },
  ],
  networks: [
    { key: 'name', label: '名称' },
    { key: 'driver', label: '驱动' },
    { key: 'subnet', label: '子网 / 网关' },
    { key: 'containers', label: '已连接容器' },
    { key: 'scope', label: '范围' },
    { key: '', label: '操作' },
  ],
  volumes: [
    { key: 'name', label: '名称' },
    { key: 'driver', label: '驱动' },
    { key: 'mountpoint', label: '挂载点' },
    { key: 'created', label: '创建时间' },
    { key: '', label: '操作' },
  ],
}

function toggleDockerSort(view: DockerTable, key: string) {
  if (colResizing) return
  const state = dockerSort[view]
  if (state.key === key) state.dir = state.dir === 'asc' ? 'desc' : 'asc'
  else {
    state.key = key
    state.dir = 'asc'
  }
}

const CONTAINER_STATE_RANK: Record<string, number> = {
  running: 0,
  restarting: 1,
  paused: 2,
  created: 3,
  exited: 4,
  dead: 5,
  removing: 6,
}

function imageSizeBytes(size: string): number {
  const m = /^([\d.]+)\s*(B|KB|MB|GB|TB)?$/i.exec(size.trim())
  if (!m) return 0
  const unit = (m[2] ?? 'B').toUpperCase()
  const mult: Record<string, number> = { B: 1, KB: 1024, MB: 1024 ** 2, GB: 1024 ** 3, TB: 1024 ** 4 }
  return Number(m[1]) * (mult[unit] ?? 1)
}

function containerSortValue(c: DockerContainer, key: string): string | number {
  switch (key) {
    case 'name': return c.name
    case 'image': return c.image
    case 'state': return CONTAINER_STATE_RANK[c.state] ?? 99
    case 'ports': return c.ports
    case 'created': return c.created
    default: return ''
  }
}

function imageSortValue(img: DockerImage, key: string): string | number {
  switch (key) {
    case 'repo': return imageRef(img)
    case 'id': return img.id
    case 'size': return imageSizeBytes(img.size)
    case 'created': return img.createdAt || img.created
    default: return ''
  }
}

function networkSortValue(n: DockerNetwork, key: string): string | number {
  switch (key) {
    case 'name': return n.name
    case 'driver': return n.driver
    case 'subnet': return n.subnet
    case 'containers': return n.containers.length
    case 'scope': return n.scope
    default: return ''
  }
}

function volumeSortValue(v: DockerVolume, key: string): string | number {
  switch (key) {
    case 'name': return v.name
    case 'driver': return v.driver
    case 'mountpoint': return v.mountpoint
    case 'created': return v.createdAt
    default: return ''
  }
}

function sortDockerRows<T>(
  rows: T[], state: DockerSortState, value: (row: T, key: string) => string | number,
): T[] {
  if (!state.key) return rows
  const dir = state.dir === 'asc' ? 1 : -1
  return [...rows].sort((a, b) => {
    const va = value(a, state.key)
    const vb = value(b, state.key)
    if (typeof va === 'number' && typeof vb === 'number') return (va - vb) * dir
    return String(va).localeCompare(String(vb), 'zh-CN', { numeric: true, sensitivity: 'base' }) * dir
  })
}

const sortedContainers = computed(() =>
  sortDockerRows(filteredContainers.value, dockerSort.containers, containerSortValue))
const sortedImages = computed(() =>
  sortDockerRows(filteredImages.value, dockerSort.images, imageSortValue))
const sortedNetworks = computed(() =>
  sortDockerRows(filteredNetworks.value, dockerSort.networks, networkSortValue))
const sortedVolumes = computed(() =>
  sortDockerRows(filteredVolumes.value, dockerSort.volumes, volumeSortValue))

/** 首列到操作列前的像素宽度；操作列保持自适应，拖动后总宽仍填满容器 */
const dockerColWidths = reactive<Record<DockerTable, number[] | null>>({
  containers: null,
  images: null,
  networks: null,
  volumes: null,
})
const DOCKER_OPS_MIN: Record<DockerTable, number> = {
  containers: 170,
  images: 76,
  networks: 56,
  volumes: 56,
}

function dockerGridStyle(view: DockerTable) {
  const widths = dockerColWidths[view]
  if (!widths) return undefined
  const fixed = widths.slice(0, -1).map(w => `${Math.round(w * 100) / 100}px`)
  return { gridTemplateColumns: [...fixed, `minmax(${DOCKER_OPS_MIN[view]}px, 1fr)`].join(' ') }
}

let colDrag: { view: DockerTable, index: number, startX: number, widths: number[] } | null = null
let colResizing = false

function startColResize(view: DockerTable, e: PointerEvent, index: number) {
  const head = (e.currentTarget as HTMLElement).closest('.dk-head')
  if (!head) return
  const widths = Array.from(head.children).map(el => (el as HTMLElement).getBoundingClientRect().width)
  dockerColWidths[view] = widths
  colDrag = { view, index, startX: e.clientX, widths: [...widths] }
  colResizing = true
  document.body.style.cursor = 'col-resize'
  document.body.style.userSelect = 'none'
  window.addEventListener('pointermove', onColResize)
  window.addEventListener('pointerup', endColResize, { once: true })
}

function onColResize(e: PointerEvent) {
  if (!colDrag) return
  const { view, index, startX, widths } = colDrag
  const minOf = (i: number) => i === widths.length - 1 ? DOCKER_OPS_MIN[view] : 48
  const next = [...widths]
  const raw = e.clientX - startX
  if (raw > 0) {
    // 加宽：从右侧列依次借空间，借不到则停在该上限，总宽保持不变
    let remain = raw
    for (let i = index + 1; i < next.length && remain > 0; i++) {
      const give = Math.min(remain, Math.max(0, next[i] - minOf(i)))
      next[i] -= give
      remain -= give
    }
    next[index] += raw - remain
  } else if (raw < 0) {
    // 收窄：腾出的空间交给右邻列
    const w = Math.max(48, widths[index] + raw)
    next[index + 1] += widths[index] - w
    next[index] = w
  }
  dockerColWidths[view] = next
}

function endColResize() {
  colDrag = null
  document.body.style.cursor = ''
  document.body.style.userSelect = ''
  window.removeEventListener('pointermove', onColResize)
  // 松开后浏览器还会补一次 click，避免误触排序
  setTimeout(() => { colResizing = false }, 0)
}

/** Docker 预置网络不可删除 */
const BUILTIN_NETWORKS = ['bridge', 'host', 'none']
const NETWORK_DRIVERS = ['bridge', 'overlay', 'macvlan', 'ipvlan']

function isBuiltinNetwork(n: DockerNetwork) {
  return BUILTIN_NETWORKS.includes(n.name)
}

function networkContainersLabel(n: DockerNetwork) {
  return n.containers.length ? n.containers.join('、') : '—'
}

/** 匿名卷由 Docker 生成 64 位十六进制名称 */
function isAnonymousVolume(v: DockerVolume) {
  return /^[0-9a-f]{64}$/.test(v.name)
}

/** volume inspect 的 CreatedAt 为 ISO 8601，压缩到分钟精度便于列展示 */
function volumeCreatedLabel(v: DockerVolume) {
  const raw = v.createdAt.trim()
  if (!raw) return '—'
  const m = /^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2})/.exec(raw)
  return m ? `${m[1]}-${m[2]}-${m[3]} ${m[4]}:${m[5]}` : raw
}

const dockerCountLabel = computed(() => {
  if (!docker.value) return ''
  if (dockerView.value === 'containers') {
    const total = dockerContainers.value.length
    const shown = filteredContainers.value.length
    return shown === total ? `共 ${total} 个容器` : `显示 ${shown} / ${total} 个容器`
  }
  if (dockerView.value === 'networks') {
    const total = dockerNetworks.value.length
    const shown = filteredNetworks.value.length
    return shown === total ? `共 ${total} 个网络` : `显示 ${shown} / ${total} 个网络`
  }
  if (dockerView.value === 'volumes') {
    const total = dockerVolumes.value.length
    const shown = filteredVolumes.value.length
    return shown === total ? `共 ${total} 个数据卷` : `显示 ${shown} / ${total} 个数据卷`
  }
  const total = dockerImages.value.length
  const shown = filteredImages.value.length
  return shown === total ? `共 ${total} 个镜像` : `显示 ${shown} / ${total} 个镜像`
})

const dockerVersionLabel = computed(() => {
  const d = docker.value
  if (!d) return ''
  const parts: string[] = []
  if (d.dockerVersion) parts.push(`Docker ${d.dockerVersion}`)
  parts.push(d.composeVersion ? `Compose ${d.composeVersion}` : 'Compose 未安装')
  return parts.join(' · ')
})

/* ---- Docker：日志浮层 ---- */
const dockerLogs = ref<{ container: string } | null>(null)
const logs = ref<DockerLogsResult | null>(null)
const logsTail = ref(200)
const logsTimestamps = ref(false)
const logsBusy = ref(false)
const logsCopied = ref(false)
const LOG_TAIL_OPTIONS = [100, 200, 500, 1000, 2000, 5000]

async function loadDockerLogs() {
  const sid = backendId.value
  const target = dockerLogs.value
  if (!sid || !target) return
  logsBusy.value = true
  logsCopied.value = false
  error.value = ''
  try {
    logs.value = await runElevated(pw => invoke<DockerLogsResult>('toolbox_docker_logs', {
      sessionId: sid,
      container: target.container,
      tail: logsTail.value,
      timestamps: logsTimestamps.value,
      sudoPassword: pw,
    }))
  } catch (e) {
    reportError(e)
  } finally {
    logsBusy.value = false
  }
}

function openDockerLogs(c: DockerContainer) {
  dockerTerm.value = null
  dockerLogs.value = { container: c.name }
  logs.value = null
  void loadDockerLogs()
}

function closeDockerLogs() {
  dockerLogs.value = null
  logs.value = null
}

async function copyDockerLogs() {
  if (!logs.value?.output) return
  if (await copyText(logs.value.output)) {
    logsCopied.value = true
    setTimeout(() => { logsCopied.value = false }, 1500)
  }
}

function logsFileName(container: string) {
  const d = new Date()
  const p = (n: number) => String(n).padStart(2, '0')
  const stamp = `${d.getFullYear()}${p(d.getMonth() + 1)}${p(d.getDate())}_${p(d.getHours())}${p(d.getMinutes())}${p(d.getSeconds())}`
  const safe = container.replace(/[^A-Za-z0-9._-]+/g, '_') || 'container'
  return `${safe}_${stamp}.log`
}

async function downloadDockerLogs() {
  const target = dockerLogs.value
  const content = logs.value?.output
  if (!target || !content) return
  const path = await saveDialog({
    title: '保存容器日志',
    defaultPath: logsFileName(target.container),
    filters: [{ name: '日志文件', extensions: ['log', 'txt'] }],
  })
  if (!path) return
  try {
    const abs = await invoke<string>('toolbox_docker_logs_save', { path, content })
    toast(`日志已保存：${abs}`, 'ok', 2800)
  } catch (e) {
    reportError(e)
  }
}

/* ---- Docker：容器交互终端 ---- */
const dockerTerm = ref<{ container: string } | null>(null)

function openDockerTerm(c: DockerContainer) {
  dockerLogs.value = null
  dockerTerm.value = { container: c.name }
}

function closeDockerTerm() {
  dockerTerm.value = null
}

/** 由 DockerTerminal 注入：创建帧通道并完成 open（含 sudo 重试） */
async function openDockerSession(channel: DockerTermChannel, cols: number, rows: number) {
  const sid = backendId.value
  const target = dockerTerm.value
  if (!sid || !target) throw new Error('当前没有已连接的会话')
  await runElevated(pw => invoke('toolbox_docker_terminal_open', {
    sessionId: sid,
    container: target.container,
    cols,
    rows,
    sudoPassword: pw,
    channel,
  }))
}

/* ---- Docker：镜像拉取 / 导入 / 导出 ---- */
interface DockerTransferProgress {
  taskId: number
  transferred: number
  total: number
  speed: number
  done: boolean
}

const transfer = ref<{
  kind: 'export' | 'import' | 'pull'
  /** 本地：原生文件选择 + SFTP 中转；远端：直接读写远端路径 */
  mode: 'local' | 'remote'
  image?: string
  path: string
  /** 导入时选中的本地文件（选文件夹时展开为其中全部镜像文件） */
  localFiles: string[]
  /** 导出时选中的本地目标（选文件夹时为「文件夹/镜像名.tar」） */
  localTarget: string
  /** 目标由「选择文件夹」得出：覆盖前需自行确认（另存为已由系统确认） */
  localFolder: boolean
  tag: string
  busy: boolean
  /** 本地中转时的阶段文案与百分比 */
  stage: string
  pct: number
} | null>(null)

let transferTaskSeq = 0
const nextTransferTask = () => ++transferTaskSeq

const transferTitle = computed(() => {
  const kind = transfer.value?.kind
  if (kind === 'export') return '导出镜像'
  if (kind === 'import') return '导入镜像'
  return '拉取镜像'
})

/** docker load 可识别的压缩包扩展名 */
const TAR_EXT = /\.(tar|tar\.gz|tgz|tar\.xz|txz|tar\.bz2|tbz2|tar\.zst|tar\.zstd)$/i

function exportFileName(image?: string) {
  const name = (image ?? 'image').replace(/[^A-Za-z0-9._-]+/g, '_') || 'image'
  return `${name}.tar`
}

/** 拼接本地路径：沿用目录自身的分隔符（Windows 反斜杠） */
function joinLocal(dir: string, name: string) {
  const sep = dir.includes('\\') && !dir.includes('/') ? '\\' : '/'
  return dir.endsWith('/') || dir.endsWith('\\') ? dir + name : dir + sep + name
}

const importSourceLabel = computed(() => {
  const t = transfer.value
  if (!t || t.kind !== 'import' || !t.localFiles.length) return ''
  return t.localFiles.length === 1 ? t.localFiles[0]! : `${t.localFiles.length} 个文件`
})

const tagDisabled = computed(() => {
  const t = transfer.value
  if (!t || t.busy) return true
  return t.kind === 'import' && t.mode === 'local' && t.localFiles.length > 1
})

function startExport(img: DockerImage) {
  const name = imageRef(img).replace(/[^A-Za-z0-9._-]+/g, '_') || 'image'
  transfer.value = {
    kind: 'export', mode: 'local', image: imageRef(img),
    path: `/tmp/${name}.tar`, localFiles: [], localTarget: '', localFolder: false,
    tag: '', busy: false, stage: '', pct: 0,
  }
}

function startImport() {
  transfer.value = {
    kind: 'import', mode: 'local', path: '/tmp/image.tar',
    localFiles: [], localTarget: '', localFolder: false,
    tag: '', busy: false, stage: '', pct: 0,
  }
}

function startPull() {
  transfer.value = {
    kind: 'pull', mode: 'remote', image: '', path: '',
    localFiles: [], localTarget: '', localFolder: false,
    tag: '', busy: false, stage: '', pct: 0,
  }
}

function closeTransfer() {
  if (!transfer.value?.busy) transfer.value = null
}

async function pickExportTarget(kind: 'file' | 'folder') {
  const t = transfer.value
  if (!t || t.kind !== 'export') return
  const name = exportFileName(t.image)
  if (kind === 'file') {
    const path = await saveDialog({
      title: '导出镜像到本地',
      defaultPath: name,
      filters: [{ name: 'Docker 镜像', extensions: ['tar'] }],
    })
    if (typeof path === 'string' && path) {
      t.localTarget = path
      t.localFolder = false
    }
    return
  }
  const dir = await openDialog({ title: '选择保存文件夹', directory: true, multiple: false })
  if (typeof dir === 'string' && dir) {
    t.localTarget = joinLocal(dir, name)
    t.localFolder = true
  }
}

async function pickImportFiles() {
  const t = transfer.value
  if (!t || t.kind !== 'import') return
  const picked = await openDialog({
    title: '选择本地镜像文件',
    directory: false,
    multiple: true,
    filters: [{ name: 'Docker 镜像', extensions: ['tar', 'gz', 'tgz', 'xz', 'bz2', 'zst'] }],
  })
  const list = Array.isArray(picked) ? picked : typeof picked === 'string' ? [picked] : []
  if (list.length) {
    error.value = ''
    t.localFiles = list
  }
}

async function pickImportFolder() {
  const t = transfer.value
  if (!t || t.kind !== 'import') return
  const dir = await openDialog({ title: '选择镜像文件夹', directory: true, multiple: false })
  if (typeof dir !== 'string' || !dir) return
  try {
    const listing = await invoke<{
      path: string
      entries: { name: string; isDir: boolean }[]
    }>('list_local_dir', { path: dir })
    const files = listing.entries
      .filter(e => !e.isDir && TAR_EXT.test(e.name))
      .map(e => joinLocal(listing.path, e.name))
      .sort()
    if (!files.length) {
      error.value = '所选文件夹里没有 .tar / .tar.gz 等镜像文件'
      return
    }
    error.value = ''
    t.localFiles = files
  } catch (e) {
    reportError(e)
  }
}

async function submitTransfer() {
  const sid = backendId.value
  const t = transfer.value
  if (!sid || !t) return
  const customTag = t.tag.trim() || null

  if (t.kind === 'pull') {
    const image = (t.image ?? '').trim()
    if (!image) {
      error.value = '请填写镜像地址'
      return
    }
    t.busy = true
    error.value = ''
    try {
      const summary = await runElevated(pw => invoke<string>('toolbox_docker_pull', {
        sessionId: sid,
        image,
        customTag,
        sudoPassword: pw,
      }))
      toast(summary, 'ok', 2800)
      transfer.value = null
      await loadDocker()
    } catch (e) {
      reportError(e)
    } finally {
      if (transfer.value) transfer.value.busy = false
    }
    return
  }

  if (t.kind === 'export' && t.mode === 'local') {
    const target = t.localTarget.trim()
    if (!target) {
      error.value = '请选择本地保存位置'
      return
    }
    if (t.localFolder) {
      try {
        const meta = await invoke<{ exists: boolean }[]>('local_files_meta', { paths: [target] })
        if (meta[0]?.exists) {
          const ok = await confirmDialog(
            `本地已存在 ${target}，覆盖写入？`,
            '覆盖本地文件',
            { confirmText: '覆盖', danger: true },
          )
          if (!ok) return
        }
      } catch (e) {
        reportError(e)
        return
      }
    }
    const image = t.image
    if (!image) return
    t.busy = true
    t.stage = '正在打包镜像'
    t.pct = 0
    error.value = ''
    const channel = new Channel<DockerTransferProgress>()
    channel.onmessage = (p) => {
      if (!t.busy) return
      if (p.total > 0) {
        t.pct = Math.min(100, (p.transferred / p.total) * 100)
        t.stage = '正在下载到本地'
      }
    }
    try {
      const size = await runElevated(pw => invoke<string>('toolbox_docker_export_local', {
        sessionId: sid,
        image,
        localPath: target,
        sudoPassword: pw,
        taskId: nextTransferTask(),
        channel,
      }))
      toast(`镜像已导出到 ${target}（${size}）`, 'ok', 3200)
      transfer.value = null
    } catch (e) {
      reportError(e)
    } finally {
      if (transfer.value) {
        transfer.value.busy = false
        transfer.value.stage = ''
        transfer.value.pct = 0
      }
    }
    return
  }

  if (t.kind === 'import' && t.mode === 'local') {
    if (!t.localFiles.length) {
      error.value = '请选择本地镜像文件或文件夹'
      return
    }
    const total = t.localFiles.length
    const tag = total === 1 ? customTag : null
    t.busy = true
    t.stage = ''
    t.pct = 0
    error.value = ''
    let done = 0
    let last = ''
    try {
      // 逐个上传导入；成功的文件即时从列表移除，失败后重试只重传剩余文件
      while (t.localFiles.length) {
        const file = t.localFiles[0]!
        const idx = done + 1
        t.stage = total > 1 ? `上传 ${idx}/${total}` : '正在上传'
        t.pct = 0
        const channel = new Channel<DockerTransferProgress>()
        channel.onmessage = (p) => {
          if (!t.busy) return
          if (p.total > 0) t.pct = Math.min(100, (p.transferred / p.total) * 100)
          if (p.done) {
            t.pct = 100
            t.stage = total > 1 ? `导入 ${idx}/${total}` : '正在导入'
          }
        }
        last = await runElevated(pw => invoke<string>('toolbox_docker_import_local', {
          sessionId: sid,
          localPath: file,
          customTag: tag,
          sudoPassword: pw,
          taskId: nextTransferTask(),
          channel,
        }))
        done++
        t.localFiles = t.localFiles.filter(f => f !== file)
      }
      toast(total > 1 ? `已导入 ${total} 个镜像文件` : last, 'ok', 2800)
      transfer.value = null
      await loadDocker()
    } catch (e) {
      reportError(e)
    } finally {
      if (transfer.value) {
        transfer.value.busy = false
        transfer.value.stage = ''
        transfer.value.pct = 0
      }
    }
    return
  }

  const path = t.path.trim()
  if (!path) {
    error.value = '请填写远端文件路径'
    return
  }
  t.busy = true
  error.value = ''
  try {
    if (t.kind === 'export' && t.image) {
      const image = t.image
      const run = (overwrite: boolean) => runElevated(pw =>
        invoke<string>('toolbox_docker_export', {
          sessionId: sid,
          image,
          path,
          overwrite,
          sudoPassword: pw,
        }),
      )
      let size: string
      try {
        size = await run(false)
      } catch (e) {
        if (errText(e) !== 'DOCKER_EXPORT_EXISTS') throw e
        const ok = await confirmDialog(
          `远端已存在 ${path}，覆盖写入？`,
          '覆盖导出文件',
          { confirmText: '覆盖', danger: true },
        )
        if (!ok) {
          t.busy = false
          return
        }
        size = await run(true)
      }
      toast(`镜像已导出到 ${path}（${size}）`, 'ok', 2800)
      transfer.value = null
    } else {
      const summary = await runElevated(pw => invoke<string>('toolbox_docker_import', {
        sessionId: sid,
        path,
        customTag,
        sudoPassword: pw,
      }))
      toast(summary, 'ok', 2800)
      transfer.value = null
      await loadDocker()
    }
  } catch (e) {
    reportError(e)
  } finally {
    if (transfer.value) transfer.value.busy = false
  }
}

/* ---- Docker：配置（daemon.json 可视化） ---- */
const dockerConfig = ref<DockerConfig | null>(null)
const configLoaded = ref(false)
const configBusy = ref(false)
const configDataRoot = ref('')
const configMirrors = ref<string[]>([])

function parseDaemonObject(text: string): Record<string, unknown> | null {
  try {
    const v: unknown = JSON.parse(text)
    return v && typeof v === 'object' && !Array.isArray(v)
      ? (v as Record<string, unknown>)
      : null
  } catch {
    return null
  }
}

/** daemon.json 顶层对象；文件不存在时为空对象，非法 JSON 时为 null */
const daemonObject = computed<Record<string, unknown> | null>(() => {
  const text = dockerConfig.value?.daemonJson
  if (!text) return {}
  return parseDaemonObject(text)
})

/** 本次不编辑的配置项：只读展示，保存时原样保留 */
const configOthers = computed(() => {
  const obj = daemonObject.value
  if (!obj) return []
  return Object.entries(obj)
    .filter(([k]) => k !== 'data-root' && k !== 'registry-mirrors')
    .map(([key, value]) => ({ key, text: JSON.stringify(value, null, 2) }))
})

/** 保存后 daemon.json 的预览（其他键原样保留） */
const daemonPreview = computed(() => {
  if (!configLoaded.value) return ''
  const obj: Record<string, unknown> = { ...(daemonObject.value ?? {}) }
  const root = configDataRoot.value.trim().replace(/\/+$/, '')
  if (root) obj['data-root'] = root
  else delete obj['data-root']
  const mirrors = configMirrors.value.map(m => m.trim()).filter(Boolean)
  if (mirrors.length) obj['registry-mirrors'] = mirrors
  else delete obj['registry-mirrors']
  return JSON.stringify(obj, null, 2)
})

/** 预览的手工编辑内容；null 表示跟随表单自动生成 */
const configPreviewEdit = ref<string | null>(null)

/** 保存时实际写入的 daemon.json：手工编辑优先，否则用表单生成的预览 */
const configEditorText = computed(() => {
  if (configPreviewEdit.value !== null) return configPreviewEdit.value
  if (!daemonObject.value) return dockerConfig.value?.daemonJson ?? ''
  return daemonPreview.value
})

/* 手工编辑后再改表单：把 data-root / registry-mirrors 同步进编辑内容 */
watch([configDataRoot, configMirrors], () => {
  const edit = configPreviewEdit.value
  if (edit === null) return
  const obj = parseDaemonObject(edit)
  if (!obj) return
  const root = configDataRoot.value.trim().replace(/\/+$/, '')
  if (root) obj['data-root'] = root
  else delete obj['data-root']
  const mirrors = configMirrors.value.map(m => m.trim()).filter(Boolean)
  if (mirrors.length) obj['registry-mirrors'] = mirrors
  else delete obj['registry-mirrors']
  configPreviewEdit.value = JSON.stringify(obj, null, 2)
}, { deep: true })

function resetConfigForm(cfg: DockerConfig) {
  configDataRoot.value = cfg.dataRoot ?? ''
  configMirrors.value = [...cfg.registryMirrors]
  configPreviewEdit.value = null
}

async function loadDockerConfig() {
  const sid = backendId.value
  if (!sid) return
  configBusy.value = true
  error.value = ''
  try {
    const cfg = await runElevated(pw => invoke<DockerConfig>('toolbox_docker_config_get', {
      sessionId: sid,
      sudoPassword: pw,
    }))
    dockerConfig.value = cfg
    resetConfigForm(cfg)
    configLoaded.value = true
  } catch (e) {
    reportError(e)
  } finally {
    configBusy.value = false
  }
}

async function saveDockerConfig() {
  const sid = backendId.value
  if (!sid) return
  const text = configEditorText.value.trim()
  if (!text) {
    error.value = '保存预览不能为空，至少保留 {}'
    return
  }
  if (!parseDaemonObject(text)) {
    error.value = '保存预览里不是合法的 JSON 对象，未做任何修改'
    return
  }
  const ok = await confirmDialog(
    '保存后会重启 Docker 服务，运行中的容器会短暂中断（已配置 live-restore 的主机除外）。继续？',
    '保存配置',
    { confirmText: '保存并重启', danger: true },
  )
  if (!ok) return
  configBusy.value = true
  error.value = ''
  try {
    const cfg = await runElevated(pw => invoke<DockerConfig>('toolbox_docker_config_save', {
      sessionId: sid,
      daemonJson: text,
      sudoPassword: pw,
    }))
    dockerConfig.value = cfg
    resetConfigForm(cfg)
    toast('daemon.json 已保存，Docker 服务已重启', 'ok', 2600)
    await loadDocker()
  } catch (e) {
    reportError(e)
  } finally {
    configBusy.value = false
  }
}

function setDockerView(view: DockerView) {
  dockerView.value = view
  if (view !== 'networks') netCreate.value = null
  if (view !== 'volumes') volCreate.value = null
  if (view === 'config' && !configLoaded.value) void loadDockerConfig()
}

function resetDocker() {
  dockerTerm.value = null
  dockerLogs.value = null
  logs.value = null
  transfer.value = null
  netCreate.value = null
  volCreate.value = null
  docker.value = null
  dockerConfig.value = null
  configLoaded.value = false
  configDataRoot.value = ''
  configMirrors.value = []
  configPreviewEdit.value = null
  dockerView.value = 'containers'
  dockerQuery.value = ''
  dockerStateFilter.value = 'all'
}

async function loadDocker() {
  const sid = backendId.value
  if (!sid) return
  busy.value = 'docker'
  error.value = ''
  try {
    docker.value = await runElevated(pw => invoke<DockerListResult>('toolbox_docker_list', {
      sessionId: sid,
      sudoPassword: pw,
    }))
  } catch (e) {
    reportError(e)
  } finally {
    busy.value = ''
  }
}

const CONTAINER_STARTABLE = ['exited', 'created', 'dead']

async function dockerAct(c: DockerContainer, action: 'start' | 'stop' | 'restart' | 'remove') {
  const sid = backendId.value
  if (!sid) return
  if (action === 'remove') {
    const ok = await confirmDialog(`删除容器 ${c.name}？容器数据卷不会被自动清理。`, '删除容器', {
      confirmText: '删除',
      danger: true,
    })
    if (!ok) return
  } else if (action === 'stop' || action === 'restart') {
    const label = action === 'stop' ? '停止' : '重启'
    const ok = await confirmDialog(
      action === 'stop'
        ? `停止容器 ${c.name}？容器内服务将中断。`
        : `重启容器 ${c.name}？容器内服务会短暂中断。`,
      `${label}容器`,
      { confirmText: label, danger: action === 'stop' },
    )
    if (!ok) return
  }
  busy.value = 'docker-act'
  error.value = ''
  try {
    const msg = await runElevated(pw => invoke<string>('toolbox_docker_action', {
      sessionId: sid,
      kind: 'container',
      action,
      target: c.name,
      sudoPassword: pw,
    }))
    toast(msg, 'ok', 2200)
    await loadDocker()
  } catch (e) {
    reportError(e)
  } finally {
    busy.value = ''
  }
}

async function removeImage(img: DockerImage) {
  const sid = backendId.value
  if (!sid) return
  const ref = imageRef(img)
  const ok = await confirmDialog(`删除镜像 ${ref}？该操作不可恢复。`, '删除镜像', {
    confirmText: '删除',
    danger: true,
  })
  if (!ok) return
  busy.value = 'docker-act'
  error.value = ''
  try {
    const msg = await runElevated(pw => invoke<string>('toolbox_docker_action', {
      sessionId: sid,
      kind: 'image',
      action: 'remove',
      target: ref,
      sudoPassword: pw,
    }))
    toast(msg, 'ok', 2200)
    await loadDocker()
  } catch (e) {
    reportError(e)
  } finally {
    busy.value = ''
  }
}

function openNetCreate() {
  netCreate.value = { name: '', driver: 'bridge', subnet: '', gateway: '' }
}

async function createNetwork() {
  const sid = backendId.value
  const form = netCreate.value
  if (!sid || !form) return
  const name = form.name.trim()
  if (!name) {
    error.value = '请填写网络名称'
    return
  }
  busy.value = 'docker-act'
  error.value = ''
  try {
    const msg = await runElevated(pw => invoke<string>('toolbox_docker_network_create', {
      sessionId: sid,
      name,
      driver: form.driver,
      subnet: form.subnet.trim() || null,
      gateway: form.gateway.trim() || null,
      sudoPassword: pw,
    }))
    toast(msg, 'ok', 2400)
    netCreate.value = null
    await loadDocker()
  } catch (e) {
    reportError(e)
  } finally {
    busy.value = ''
  }
}

async function removeNetwork(n: DockerNetwork) {
  const sid = backendId.value
  if (!sid || isBuiltinNetwork(n)) return
  const ok = await confirmDialog(
    n.containers.length
      ? `删除网络 ${n.name}？该网络仍连接 ${n.containers.length} 个容器，需先断开后才能删除。`
      : `删除网络 ${n.name}？该操作不可恢复。`,
    '删除网络',
    { confirmText: '删除', danger: true },
  )
  if (!ok) return
  busy.value = 'docker-act'
  error.value = ''
  try {
    const msg = await runElevated(pw => invoke<string>('toolbox_docker_action', {
      sessionId: sid,
      kind: 'network',
      action: 'remove',
      target: n.name,
      sudoPassword: pw,
    }))
    toast(msg, 'ok', 2200)
    await loadDocker()
  } catch (e) {
    reportError(e)
  } finally {
    busy.value = ''
  }
}

function openVolCreate() {
  volCreate.value = { name: '', driver: 'local' }
}

async function createVolume() {
  const sid = backendId.value
  const form = volCreate.value
  if (!sid || !form) return
  const name = form.name.trim()
  if (!name) {
    error.value = '请填写数据卷名称'
    return
  }
  busy.value = 'docker-act'
  error.value = ''
  try {
    const msg = await runElevated(pw => invoke<string>('toolbox_docker_volume_create', {
      sessionId: sid,
      name,
      driver: form.driver.trim(),
      sudoPassword: pw,
    }))
    toast(msg, 'ok', 2400)
    volCreate.value = null
    await loadDocker()
  } catch (e) {
    reportError(e)
  } finally {
    busy.value = ''
  }
}

async function removeVolume(v: DockerVolume) {
  const sid = backendId.value
  if (!sid) return
  const ok = await confirmDialog(
    `删除数据卷 ${v.name}？卷内数据会被一并删除，且不可恢复。正在被容器使用的卷需先移除容器后才能删除。`,
    '删除数据卷',
    { confirmText: '删除', danger: true },
  )
  if (!ok) return
  busy.value = 'docker-act'
  error.value = ''
  try {
    const msg = await runElevated(pw => invoke<string>('toolbox_docker_action', {
      sessionId: sid,
      kind: 'volume',
      action: 'remove',
      target: v.name,
      sudoPassword: pw,
    }))
    toast(msg, 'ok', 2200)
    await loadDocker()
  } catch (e) {
    reportError(e)
  } finally {
    busy.value = ''
  }
}

function errText(e: unknown): string {
  if (typeof e === 'string') return e
  if (e instanceof Error) return e.message
  return String(e)
}

/** 用户取消 sudo 输入不算错误，不占用错误横幅 */
function reportError(e: unknown) {
  const msg = errText(e)
  if (msg !== '已取消输入 sudo 密码') error.value = msg
}

/**
 * 提权调用：首次自动走 `sudo -n`，失败弹窗问密码并重试一次。
 * 密码只存内存（toolboxSudo），错误口令立即从缓存剔除。
 */
async function runElevated<T>(fn: (password?: string) => Promise<T>): Promise<T> {
  const sid = backendId.value
  if (!sid) throw new Error('当前没有已连接的会话')
  const cached = cachedSudo(sid)
  try {
    return await fn(cached)
  } catch (e) {
    const msg = errText(e)
    if (msg !== SUDO_REQUIRED && msg !== SUDO_INCORRECT) throw e
    if (msg === SUDO_INCORRECT) forgetSudo(sid)
    const pw = await promptPassword(
      `「${sessionLabel.value}」执行该操作需要 root 权限。密码仅保存在本机内存，断开连接即清除。`,
      '需要 sudo 权限',
      '输入 sudo 密码',
    )
    if (pw === null) throw new Error('已取消输入 sudo 密码')
    if (!pw.trim()) throw new Error('sudo 密码不能为空')
    rememberSudo(sid, pw)
    try {
      return await fn(pw)
    } catch (e2) {
      if (errText(e2) === SUDO_INCORRECT) {
        forgetSudo(sid)
        throw new Error('sudo 密码错误，请重新输入')
      }
      throw e2
    }
  }
}

async function loadFirewall() {
  if (!backendId.value) return
  busy.value = 'fw'
  error.value = ''
  try {
    firewall.value = await runElevated(pw => invoke<FirewallStatus>('toolbox_firewall_status', {
      sessionId: backendId.value,
      sudoPassword: pw,
    }))
  } catch (e) {
    reportError(e)
  } finally {
    busy.value = ''
  }
}

async function loadPorts(admin = false) {
  const sid = backendId.value
  if (!sid) return
  busy.value = 'ports'
  error.value = ''
  try {
    const elevate = admin || cachedSudo(sid) !== undefined
    ports.value = elevate
      ? await runElevated(pw => invoke<PortsResult>('toolbox_list_ports', {
        sessionId: sid,
        sudoPassword: pw,
        elevate: true,
      }))
      : await invoke<PortsResult>('toolbox_list_ports', { sessionId: sid })
  } catch (e) {
    reportError(e)
  } finally {
    busy.value = ''
  }
}

async function toggleFirewall() {
  if (!firewall.value || !backendId.value) return
  const enable = !firewall.value.enabled
  if (!enable) {
    const ok = await confirmDialog(
      `停用后 ${firewall.value.backend} 将不再过滤任何入站连接，本机端口会按默认策略暴露。确认停用？`,
      '停用防火墙',
      { confirmText: '停用', danger: true },
    )
    if (!ok) return
  }
  busy.value = 'fw-toggle'
  error.value = ''
  try {
    const res = await runElevated(pw => invoke<ToggleResult>('toolbox_firewall_toggle', {
      sessionId: backendId.value,
      enable,
      sshPort: sshPort.value,
      sudoPassword: pw,
    }))
    firewall.value = res.status
    if (!enable) {
      toast('防火墙已停用', 'ok')
    } else if (res.sshPortAutoOpened) {
      toast(`防火墙已启用，并自动放行 SSH 端口 ${sshPort.value}`, 'ok', 2600)
    } else {
      toast('防火墙已启用', 'ok')
    }
  } catch (e) {
    reportError(e)
  } finally {
    busy.value = ''
  }
}

async function addRule() {
  const sid = backendId.value
  if (!sid) return
  const port = form.port.trim()
  if (!port) {
    error.value = '请填写端口（示例 80 或 8080:8090）'
    return
  }
  busy.value = 'add'
  error.value = ''
  try {
    const msg = await runElevated(pw => invoke<string>('toolbox_firewall_add', {
      sessionId: sid,
      port,
      protocol: form.protocol,
      action: form.action,
      source: form.source.trim() || null,
      sudoPassword: pw,
    }))
    toast(msg, 'ok')
    form.port = ''
    form.source = ''
    await loadFirewall()
  } catch (e) {
    reportError(e)
  } finally {
    busy.value = ''
  }
}

async function removeRule(rule: FirewallRule) {
  const sid = backendId.value
  if (!sid || !rule.port) return
  const ok = await confirmDialog(`删除规则：${rule.raw}？`, '删除防火墙规则', {
    confirmText: '删除',
    danger: true,
  })
  if (!ok) return
  busy.value = 'remove'
  error.value = ''
  try {
    const msg = await runElevated(pw => invoke<string>('toolbox_firewall_remove', {
      sessionId: sid,
      port: rule.port,
      protocol: rule.protocol ?? 'both',
      action: rule.action,
      source: rule.source ?? null,
      origin: rule.origin,
      sudoPassword: pw,
    }))
    toast(msg, 'ok')
    await loadFirewall()
  } catch (e) {
    reportError(e)
  } finally {
    busy.value = ''
  }
}

function switchTab(next: Tab) {
  tab.value = next
  error.value = ''
  pubkeyResult.value = null
  if (next === 'firewall' && !firewall.value) void loadFirewall()
  if (next === 'ports' && !ports.value) void loadPorts()
  if (next === 'docker') {
    if (dockerView.value === 'config') {
      if (!configLoaded.value) void loadDockerConfig()
    } else if (!docker.value) {
      void loadDocker()
    }
  }
}

function retryCurrent() {
  if (tab.value === 'firewall') void loadFirewall()
  else if (tab.value === 'ports') void loadPorts()
  else if (tab.value === 'docker') {
    if (dockerView.value === 'config') void loadDockerConfig()
    else void loadDocker()
  } else void deployPubkey()
}

/** 打开 / 切会话后只预载当前页签，避免无谓的远端采集 */
function loadCurrentTab() {
  if (!sessionReady.value) return
  if (tab.value === 'firewall') void loadFirewall()
  else if (tab.value === 'ports') void loadPorts()
  else if (tab.value === 'docker') {
    if (dockerView.value === 'config') void loadDockerConfig()
    else void loadDocker()
  }
}

/* 打开时只拉当前页；切会话时重新拉，避免展示上一台主机的数据 */
let openedOnce = false
function openToolbox() {
  if (!openedOnce) openedOnce = true
  firewall.value = null
  ports.value = null
  error.value = ''
  resetPubkeyForm()
  resetDocker()
  loadCurrentTab()
}
watch(showToolbox, v => { if (v) openToolbox() })
// 懒挂载：开屏初始化放 onMounted（setup 完成后），immediate watch 会引用后声明的 const 触发 TDZ
onMounted(() => { if (showToolbox.value) openToolbox() })
watch(backendId, () => {
  if (!showToolbox.value) return
  firewall.value = null
  ports.value = null
  error.value = ''
  resetPubkeyForm()
  resetDocker()
  loadCurrentTab()
})

function close() {
  // 先卸掉 Docker 浮层：终端组件卸载时会关闭远端 docker exec 通道
  dockerTerm.value = null
  dockerLogs.value = null
  transfer.value = null
  showToolbox.value = false
}

function onKey(e: KeyboardEvent) {
  // 密码/确认弹窗叠在工具箱之上时，Esc 只关闭上层
  if (passwordPromptState.visible.value || confirmState.visible.value) return
  if (showToolbox.value && e.key === 'Escape') {
    e.preventDefault()
    close()
  }
}
document.addEventListener('keydown', onKey)
onUnmounted(() => {
  document.removeEventListener('keydown', onKey)
  if (colDrag) endColResize()
})
</script>

<template>
  <Teleport to="body">
    <div class="mask" :class="{ show: showToolbox }" @click.self="close">
      <div class="modal tbx-modal" role="dialog" aria-modal="true" aria-labelledby="tbxTitle">
        <div class="modal-head">
          <div class="modal-icon">
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
                 stroke-linecap="round" stroke-linejoin="round">
              <path d="M14.7 6.3a4 4 0 00-5.4 5.4L4 17v3h3l5.3-5.3a4 4 0 005.4-5.4l-2.3 2.3-2.4-2.4 2.7-2.9z" />
            </svg>
          </div>
          <div class="modal-title">
            <h2 id="tbxTitle">工具箱</h2>
            <p>{{ sessionLabel }} · Docker、防火墙、端口、公钥、系统日志与时间同步</p>
          </div>
          <button class="modal-close" title="关闭 (Esc)" @click="close">
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2"
                 stroke-linecap="round">
              <line x1="18" y1="6" x2="6" y2="18" />
              <line x1="6" y1="6" x2="18" y2="18" />
            </svg>
          </button>
        </div>

        <div class="tbx-tabs">
          <button class="tbx-tab" :class="{ active: tab === 'docker' }" @click="switchTab('docker')">
            Docker
          </button>
          <button class="tbx-tab" :class="{ active: tab === 'firewall' }" @click="switchTab('firewall')">
            防火墙
          </button>
          <button class="tbx-tab" :class="{ active: tab === 'ports' }" @click="switchTab('ports')">
            端口
          </button>
          <button class="tbx-tab" :class="{ active: tab === 'pubkey' }" @click="switchTab('pubkey')">
            公钥
          </button>
          <button class="tbx-tab" :class="{ active: tab === 'syslog' }" @click="switchTab('syslog')">
            日志
          </button>
          <button class="tbx-tab" :class="{ active: tab === 'time' }" @click="switchTab('time')">
            时间
          </button>
        </div>

        <div class="tbx-body">
          <div v-if="!sessionReady" class="tbx-empty">请先连接主机再使用工具箱</div>

          <div v-else-if="error" class="tbx-error">
            <span>{{ error }}</span>
            <button class="tbx-link" @click="retryCurrent">{{ tab === 'pubkey' ? '重试写入' : '重试' }}</button>
            <button class="tbx-error-close" title="关闭" aria-label="关闭" @click="error = ''">×</button>
          </div>

          <!-- 系统日志（自持来源探测 / 筛选 / 跟随 / 错误处理） -->
          <SyslogPanel
            v-if="sessionReady && tab === 'syslog'"
            :session-id="backendId ?? ''"
            :run-elevated="runElevated"
          />

          <!-- 服务器时间与 NTP（自持时钟 / 状态 / 提权操作） -->
          <TimePanel
            v-if="sessionReady && tab === 'time'"
            :session-id="backendId ?? ''"
            :run-elevated="runElevated"
          />

          <!-- 防火墙 -->
          <template v-if="sessionReady && tab === 'firewall'">
            <div class="fw-title">
              <h3>防火墙</h3>
              <span class="spacer"></span>
              <button class="tbx-btn" :disabled="!!busy" @click="loadFirewall">刷新</button>
            </div>

            <div class="fw-status">
              <span v-if="firewall" class="fw-pill" :class="{ on: firewall.enabled }">{{ fwBackendLabel }}</span>
              <span v-else class="fw-pill">{{ fwBackendLabel }}</span>
              <span v-if="firewall && firewall.backend !== 'none'" class="fw-status-text" :class="{ on: firewall.enabled }">
                — {{ busy === 'fw-toggle'
                  ? (firewall.enabled ? '正在停用…' : '正在启用…')
                  : (firewall.enabled ? '已启用' : '未启用') }}
              </span>
              <span class="spacer"></span>
              <template v-if="firewall && firewall.backend !== 'none'">
                <span class="fw-switch-text">{{ busy === 'fw-toggle' ? '处理中' : (firewall.enabled ? '开' : '关') }}</span>
                <button
                  class="fw-switch" :class="{ on: firewall.enabled, busy: busy === 'fw-toggle' }"
                  role="switch" :aria-checked="firewall.enabled"
                  :disabled="!!busy || !canToggleFirewall"
                  :title="canToggleFirewall ? (firewall.enabled ? '停用防火墙' : '启用防火墙') : '当前后端不支持启停'"
                  @click="toggleFirewall"
                >
                  <span class="fw-knob"></span>
                </button>
              </template>
              <span
                v-if="busy === 'fw-toggle'" class="fw-progress"
                role="progressbar" aria-valuetext="正在切换防火墙状态"
              >
                <span class="fw-progress-bar"></span>
              </span>
            </div>

            <div class="fw-stats">
              <div class="fw-stat total">
                <span class="fw-stat-label">规则总数</span>
                <strong>{{ firewall ? fwStats.total : '—' }}</strong>
              </div>
              <div class="fw-stat allow">
                <span class="fw-stat-label">允许</span>
                <strong>{{ firewall ? fwStats.allow : '—' }}</strong>
              </div>
              <div class="fw-stat deny">
                <span class="fw-stat-label">拒绝</span>
                <strong>{{ firewall ? fwStats.deny : '—' }}</strong>
              </div>
              <div class="fw-stat other">
                <span class="fw-stat-label">其他</span>
                <strong>{{ firewall ? fwStats.other : '—' }}</strong>
              </div>
            </div>

            <p v-if="firewall?.note" class="fw-note">{{ firewall.note }}</p>

            <div class="fw-filter">
              <input
                v-model="fwQuery" class="tbx-input" placeholder="搜索端口 / 协议 / 来源…"
                autocomplete="off" spellcheck="false"
              />
              <select v-model="fwActionFilter" class="tbx-input tbx-select" title="动作筛选">
                <option value="all">全部动作</option>
                <option value="allow">允许</option>
                <option value="deny">拒绝</option>
                <option value="other">其他</option>
              </select>
              <span class="spacer"></span>
              <button
                class="tbx-btn"
                :disabled="!firewall || firewall.backend === 'none' || !firewall.enabled"
                :title="!firewall || firewall.backend === 'none' ? '未检测到可管理的防火墙' : (!firewall.enabled ? '请先启用防火墙' : showAddForm ? '收起表单' : '新增规则')"
                @click="showAddForm = !showAddForm"
              >{{ showAddForm ? '收起' : '＋ 添加规则' }}</button>
            </div>

            <div v-if="showAddForm" class="fw-form">
              <select v-model="form.action" class="tbx-input tbx-select" title="动作">
                <option value="allow">允许</option>
                <option value="deny">拒绝</option>
              </select>
              <input v-model="form.port" class="tbx-input" placeholder="端口，如 80 或 8080:8090" autocomplete="off" />
              <select v-model="form.protocol" class="tbx-input tbx-select" title="协议">
                <option value="tcp">TCP</option>
                <option value="udp">UDP</option>
                <option value="both">TCP+UDP</option>
              </select>
              <input v-model="form.source" class="tbx-input" placeholder="来源 IP/CIDR（可选）" autocomplete="off" />
              <button
                class="tbx-btn primary"
                :disabled="!!busy || !firewall || !firewall.enabled"
                @click="addRule"
              >
                {{ busy === 'add' ? '添加中…' : '添加规则' }}
              </button>
            </div>

            <div class="fw-head">
              <span>动作</span><span>端口</span><span>协议</span><span>来源</span>
              <span class="grow">规则</span><span>操作</span>
            </div>
            <div class="fw-list">
              <div v-if="busy === 'fw'" class="tbx-empty">正在读取防火墙规则…</div>
              <div v-else-if="!firewall" class="tbx-empty">尚未读取</div>
              <div v-else-if="firewall.backend === 'none'" class="tbx-empty">未检测到可管理的防火墙后端</div>
              <div v-else-if="!firewall.enabled" class="tbx-empty">防火墙已禁用，开启后可管理规则。</div>
              <div v-else-if="!firewallRows.length" class="tbx-empty">没有可显示的规则</div>
              <div v-else-if="!filteredFirewallRows.length" class="tbx-empty">没有匹配筛选条件的规则</div>
              <div v-for="(r, i) in filteredFirewallRows" :key="`${r.origin}-${r.raw}-${i}`" class="fw-row">
                <span class="fw-action" :class="r.action">{{ r.action === 'allow' ? '允许' : r.action === 'deny' ? '拒绝' : '其他' }}</span>
                <span class="mono">{{ r.port ?? '—' }}</span>
                <span>{{ r.protocol ?? '—' }}</span>
                <span class="ellipsis" :title="r.source">{{ r.source ?? '任意' }}</span>
                <span class="ellipsis grow" :title="r.raw">{{ r.raw }}</span>
                <span>
                  <button
                    v-if="r.deletable" class="tbx-link danger" :disabled="!!busy"
                    @click="removeRule(r)"
                  >删除</button>
                  <span v-else class="muted">只读</span>
                </span>
              </div>
            </div>
          </template>

          <!-- 端口 -->
          <template v-if="sessionReady && tab === 'ports'">
            <div class="pt-bar">
              <input v-model="portQuery" class="tbx-input" placeholder="搜索端口 / 进程 / 用户 / 容器 / 地址" autocomplete="off" spellcheck="false" />
              <select v-model="protoFilter" class="tbx-input tbx-select" title="协议筛选">
                <option value="all">全部协议</option>
                <option value="tcp">TCP</option>
                <option value="udp">UDP</option>
              </select>
              <span v-if="ports" class="pt-tool">
                {{ ports.tool }} · {{ ports.elevated ? 'root 视图' : '当前用户视图' }}
              </span>
              <span class="spacer"></span>
              <span v-if="ports" class="pt-count">{{ portsCountLabel }}</span>
              <button
                v-if="ports && !ports.elevated" class="tbx-btn" :disabled="!!busy"
                title="以 root 采集，可看到全部进程的归属"
                @click="loadPorts(true)"
              >以管理员权限刷新</button>
              <button class="tbx-btn" :disabled="!!busy" @click="loadPorts()">刷新</button>
            </div>

            <div class="pt-head">
              <span>端口</span><span>协议</span><span>监听地址</span><span>进程</span>
              <span>用户</span><span>容器</span><span>PID</span>
            </div>
            <div class="pt-list">
              <div v-if="busy === 'ports'" class="tbx-empty">正在读取监听端口…</div>
              <div v-else-if="!ports" class="tbx-empty">尚未读取</div>
              <div v-else-if="!filteredPorts.length" class="tbx-empty">
                {{ ports.rows.length ? '没有匹配筛选条件的端口' : '未发现监听端口' }}
              </div>
              <div v-for="r in filteredPorts" :key="`${r.protocol}-${r.address}-${r.port}-${r.pid ?? 0}`" class="pt-row">
                <span class="mono">{{ r.port }}</span>
                <span>{{ r.protocol }}</span>
                <span class="ellipsis" :title="r.address">{{ r.address }}</span>
                <span class="ellipsis" :title="r.process">{{ r.process ?? '—' }}</span>
                <span class="ellipsis" :title="r.user">{{ r.user ?? '—' }}</span>
                <span
                  class="ellipsis"
                  :class="{ 'pt-container': r.container, 'pt-copyable': r.container }"
                  :title="r.container ? `点击复制容器名：${r.container}` : undefined"
                  @click="copyContainer(r.container)"
                >{{ r.container ?? '—' }}</span>
                <span class="mono muted">{{ r.pid ?? '—' }}</span>
              </div>
            </div>
            <p v-if="ports && !ports.elevated" class="pt-note">
              非提权视图只能看到当前用户有权查看的进程；以管理员权限刷新可显示全部进程与容器名。
            </p>
          </template>

          <!-- 公钥 -->
          <template v-if="sessionReady && tab === 'pubkey'">
            <div class="fw-title">
              <h3>写入公钥</h3>
              <span class="spacer"></span>
              <span class="pk-target-preview mono" :title="pubkeyPathPreview">{{ pubkeyPathPreview }}</span>
            </div>

            <div class="pk-form">
              <div class="pk-row">
                <label class="pk-label" for="pkKeySource">公钥</label>
                <select id="pkKeySource" v-model="keySource" class="tbx-input tbx-select pk-key">
                  <option v-for="k in keys" :key="k.id" :value="k.id">
                    {{ k.name }} · {{ k.type }} {{ k.bits }}
                  </option>
                  <option value="manual">手动粘贴公钥</option>
                </select>
              </div>

              <div v-if="selectedKey" class="pk-meta">
                <span class="mono ellipsis" :title="keyLine">{{ keyLine }}</span>
                <span class="pk-meta-sub">
                  {{ selectedKey.type }} {{ selectedKey.bits }} 位 · {{ selectedKey.comment || '无注释' }}
                </span>
              </div>
              <textarea
                v-else v-model="manualKey" class="tbx-input pk-textarea"
                placeholder="ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAA... user@host"
                spellcheck="false" autocomplete="off"
              ></textarea>

              <div class="pk-row">
                <label class="pk-label" for="pkTargetUser">目标用户</label>
                <input
                  id="pkTargetUser" v-model="targetUser" class="tbx-input pk-user"
                  placeholder="如 root、deploy" autocomplete="off" spellcheck="false"
                />
                <span class="spacer"></span>
                <button
                  class="tbx-btn primary"
                  :disabled="!!busy || !keyLine || !deployUser"
                  @click="deployPubkey"
                >{{ busy === 'pubkey' ? '写入中…' : '写入公钥' }}</button>
              </div>

              <p class="pk-hint">
                公钥会追加到目标用户的 <code>~/.ssh/authorized_keys</code>，已存在相同公钥时不会重复写入。
                写入其他用户需要 root 权限，会按需询问 sudo 密码。
              </p>
              <p v-if="!keys.length && keySource === 'manual'" class="pk-hint">
                本地还没有密钥；可在首页「密钥」页生成或导入，也可以直接粘贴公钥文本。
              </p>

              <div v-if="pubkeyResult" class="pk-result" :class="{ exists: !pubkeyResult.added }">
                <span>{{ pubkeyResult.added ? '公钥已写入' : '公钥已存在，未重复写入' }}</span>
                <code>{{ pubkeyResult.path }}</code>
              </div>
            </div>
          </template>

          <!-- Docker -->
          <template v-if="sessionReady && tab === 'docker'">
            <!-- 容器日志 -->
            <template v-if="dockerLogs">
              <div class="dk-panel-head">
                <button class="tbx-link" @click="closeDockerLogs">← 返回</button>
                <strong class="mono ellipsis dk-panel-title" :title="dockerLogs.container">
                  {{ dockerLogs.container }}
                </strong>
                <span class="spacer"></span>
                <select v-model.number="logsTail" class="tbx-input tbx-select" title="显示行数" @change="loadDockerLogs">
                  <option v-for="n in LOG_TAIL_OPTIONS" :key="n" :value="n">末尾 {{ n }} 行</option>
                </select>
                <label class="dk-check">
                  <input v-model="logsTimestamps" type="checkbox" @change="loadDockerLogs" />
                  时间戳
                </label>
                <button class="tbx-btn" :disabled="logsBusy" @click="loadDockerLogs">刷新</button>
                <button class="tbx-btn" :disabled="!logs?.output" @click="copyDockerLogs">
                  {{ logsCopied ? '已复制' : '复制' }}
                </button>
                <button class="tbx-btn" :disabled="!logs?.output" @click="downloadDockerLogs">下载</button>
              </div>
              <p v-if="logs?.truncated" class="dk-hint">日志过长，仅显示末尾部分。</p>
              <div class="dk-logs-wrap">
                <div v-if="logsBusy" class="tbx-empty">正在读取容器日志…</div>
                <pre v-else-if="logs" class="dk-logs">{{ logs.output || '（无输出）' }}</pre>
                <div v-else class="tbx-empty">尚未读取</div>
              </div>
            </template>

            <!-- 容器交互终端 -->
            <template v-else-if="dockerTerm && backendId">
              <div class="dk-panel-head">
                <button class="tbx-link" @click="closeDockerTerm">← 返回</button>
                <strong class="mono ellipsis dk-panel-title" :title="dockerTerm.container">
                  {{ dockerTerm.container }}
                </strong>
                <span class="dk-hint">容器内交互终端（优先 bash，回退 sh）</span>
                <span class="spacer"></span>
                <button class="tbx-btn" @click="closeDockerTerm">退出终端</button>
              </div>
              <DockerTerminal
                :session-id="backendId!"
                :container="dockerTerm.container"
                :open-session="openDockerSession"
                @error="reportError"
              />
            </template>

            <!-- 列表 / 配置 -->
            <template v-else>
              <div class="dk-bar">
                <div class="dk-seg">
                  <button
                    :class="{ active: dockerView === 'containers' }"
                    @click="setDockerView('containers')"
                  >
                    容器<span v-if="docker" class="dk-seg-count">{{ dockerContainers.length }}</span>
                  </button>
                  <button
                    :class="{ active: dockerView === 'images' }"
                    @click="setDockerView('images')"
                  >
                    镜像<span v-if="docker" class="dk-seg-count">{{ dockerImages.length }}</span>
                  </button>
                  <button
                    :class="{ active: dockerView === 'networks' }"
                    @click="setDockerView('networks')"
                  >
                    网络<span v-if="docker" class="dk-seg-count">{{ dockerNetworks.length }}</span>
                  </button>
                  <button
                    :class="{ active: dockerView === 'volumes' }"
                    @click="setDockerView('volumes')"
                  >
                    卷<span v-if="docker" class="dk-seg-count">{{ dockerVolumes.length }}</span>
                  </button>
                  <button
                    :class="{ active: dockerView === 'config' }"
                    @click="setDockerView('config')"
                  >
                    配置
                  </button>
                </div>
                <template v-if="dockerView !== 'config'">
                  <input
                    v-model="dockerQuery" class="tbx-input dk-search"
                    :placeholder="dockerView === 'containers'
                      ? '搜索名称 / 镜像 / 状态 / 端口…'
                      : dockerView === 'images'
                        ? '搜索仓库 / 标签 / 镜像 ID…'
                        : dockerView === 'networks'
                          ? '搜索网络名称 / 驱动 / 子网 / 容器…'
                          : '搜索卷名称 / 驱动 / 挂载点…'"
                    autocomplete="off" spellcheck="false"
                  />
                  <select
                    v-if="dockerView === 'containers'" v-model="dockerStateFilter"
                    class="tbx-input tbx-select" title="状态筛选"
                  >
                    <option value="all">全部状态</option>
                    <option value="running">运行中</option>
                    <option value="exited">已停止</option>
                    <option value="paused">已暂停</option>
                    <option value="other">其他</option>
                  </select>
                  <span class="spacer"></span>
                  <span v-if="docker" class="pt-tool">{{ dockerVersionLabel }}</span>
                  <span v-if="docker" class="pt-tool">{{ docker.elevated ? 'root 视图' : '当前用户视图' }}</span>
                  <span v-if="docker" class="pt-count">{{ dockerCountLabel }}</span>
                  <button
                    v-if="dockerView === 'images'" class="tbx-btn"
                    :disabled="!!busy || !!transfer" @click="startPull"
                  >拉取镜像</button>
                  <button
                    v-if="dockerView === 'images'" class="tbx-btn"
                    :disabled="!!busy || !!transfer" @click="startImport"
                  >导入镜像</button>
                  <button
                    v-if="dockerView === 'networks'" class="tbx-btn"
                    :disabled="!!busy" @click="netCreate ? (netCreate = null) : openNetCreate()"
                  >{{ netCreate ? '取消新建' : '新建网络' }}</button>
                  <button
                    v-if="dockerView === 'volumes'" class="tbx-btn"
                    :disabled="!!busy" @click="volCreate ? (volCreate = null) : openVolCreate()"
                  >{{ volCreate ? '取消新建' : '新建卷' }}</button>
                  <button class="tbx-btn" :disabled="!!busy" @click="loadDocker">刷新</button>
                </template>
                <template v-else>
                  <span class="spacer"></span>
                  <span v-if="dockerConfig" class="pt-tool">{{ dockerConfig.version }}</span>
                  <button class="tbx-btn" :disabled="configBusy" @click="loadDockerConfig">重新读取</button>
                </template>
              </div>

              <!-- 容器列表 -->
              <template v-if="dockerView === 'containers'">
                <div class="dk-head dk-cols-container" :style="dockerGridStyle('containers')">
                  <span
                    v-for="(col, i) in DOCKER_COLUMNS.containers" :key="col.label"
                    class="dk-th"
                    :class="{ sortable: !!col.key, sorted: !!col.key && dockerSort.containers.key === col.key }"
                    @click="col.key && toggleDockerSort('containers', col.key)"
                  >
                    {{ col.label }}
                    <span v-if="col.key && dockerSort.containers.key === col.key" class="dk-sort">
                      {{ dockerSort.containers.dir === 'asc' ? '▲' : '▼' }}
                    </span>
                    <span
                      v-if="i < DOCKER_COLUMNS.containers.length - 1" class="dk-resizer"
                      @click.stop @pointerdown.stop.prevent="startColResize('containers', $event, i)"
                    ></span>
                  </span>
                </div>
                <div class="dk-list">
                  <div v-if="busy === 'docker'" class="tbx-empty">正在读取 Docker 容器、镜像、网络与数据卷…</div>
                  <div v-else-if="!docker" class="tbx-empty">尚未读取</div>
                  <div v-else-if="!filteredContainers.length" class="tbx-empty">
                    {{ docker.containers.length ? '没有匹配筛选条件的容器' : '未发现容器' }}
                  </div>
                  <div
                    v-for="c in sortedContainers" :key="c.id" class="dk-row dk-cols-container"
                    :style="dockerGridStyle('containers')"
                  >
                    <span class="dk-name">
                      <span class="ellipsis" :title="c.name">{{ c.name }}</span>
                      <span class="dk-id mono" :title="c.id">{{ shortId(c.id) }}</span>
                    </span>
                    <span class="ellipsis mono" :title="c.image">{{ c.image }}</span>
                    <span class="dk-state" :class="c.state" :title="c.status">
                      {{ dockerStateLabel(c.state) }}
                      <span class="dk-status ellipsis">{{ c.status }}</span>
                    </span>
                    <span class="ellipsis mono" :title="c.ports">{{ c.ports || '—' }}</span>
                    <span class="ellipsis" :title="c.created">{{ c.created }}</span>
                    <span class="dk-ops">
                      <button class="tbx-link" :disabled="!!busy" @click="openDockerLogs(c)">日志</button>
                      <button
                        v-if="c.state === 'running'" class="tbx-link" :disabled="!!busy"
                        @click="openDockerTerm(c)"
                      >终端</button>
                      <button
                        v-if="c.state === 'running'" class="tbx-link" :disabled="!!busy"
                        @click="dockerAct(c, 'stop')"
                      >停止</button>
                      <button
                        v-if="c.state === 'running'" class="tbx-link" :disabled="!!busy"
                        @click="dockerAct(c, 'restart')"
                      >重启</button>
                      <button
                        v-if="CONTAINER_STARTABLE.includes(c.state)" class="tbx-link" :disabled="!!busy"
                        @click="dockerAct(c, 'start')"
                      >启动</button>
                      <button
                        v-if="CONTAINER_STARTABLE.includes(c.state)" class="tbx-link danger" :disabled="!!busy"
                        @click="dockerAct(c, 'remove')"
                      >删除</button>
                    </span>
                  </div>
                </div>
              </template>

              <!-- 镜像列表 -->
              <template v-else-if="dockerView === 'images'">
                <div v-if="transfer" class="dk-transfer">
                  <span class="dk-transfer-title">{{ transferTitle }}</span>
                  <span
                    v-if="transfer.kind === 'export' && transfer.image"
                    class="mono ellipsis dk-transfer-img" :title="transfer.image"
                  >{{ transfer.image }}</span>
                  <select
                    v-if="transfer.kind !== 'pull'" v-model="transfer.mode"
                    class="tbx-input tbx-select dk-transfer-mode" :disabled="transfer.busy"
                    :title="transfer.kind === 'export' ? '导出目标' : '镜像来源'"
                  >
                    <option value="local">{{ transfer.kind === 'export' ? '保存到本地' : '本地文件' }}</option>
                    <option value="remote">{{ transfer.kind === 'export' ? '保存到远端' : '远端路径' }}</option>
                  </select>

                  <input
                    v-if="transfer.kind === 'pull'" v-model="transfer.image"
                    class="tbx-input dk-transfer-path"
                    placeholder="镜像地址，如 harbor.example.com/team/app:1.0"
                    :disabled="transfer.busy" autocomplete="off" spellcheck="false"
                  />

                  <!-- 导出：本地文件/文件夹 或 远端路径 -->
                  <template v-else-if="transfer.kind === 'export'">
                    <template v-if="transfer.mode === 'local'">
                      <input
                        :value="transfer.localTarget" class="tbx-input dk-transfer-path mono" readonly
                        placeholder="选择本地文件或文件夹…" title="点击右侧按钮选择保存位置"
                      />
                      <button class="tbx-btn" :disabled="transfer.busy" @click="pickExportTarget('file')">
                        选择文件…
                      </button>
                      <button class="tbx-btn" :disabled="transfer.busy" @click="pickExportTarget('folder')">
                        选择文件夹…
                      </button>
                    </template>
                    <input
                      v-else v-model="transfer.path" class="tbx-input dk-transfer-path"
                      placeholder="远端保存路径，如 /tmp/nginx_latest.tar"
                      :disabled="transfer.busy" autocomplete="off" spellcheck="false"
                    />
                  </template>

                  <!-- 导入：本地文件/文件夹 或 远端路径 -->
                  <template v-else>
                    <template v-if="transfer.mode === 'local'">
                      <input
                        :value="importSourceLabel" class="tbx-input dk-transfer-path mono" readonly
                        placeholder="选择本地镜像文件或文件夹…" :title="transfer.localFiles.join('\n')"
                      />
                      <button class="tbx-btn" :disabled="transfer.busy" @click="pickImportFiles">
                        选择文件…
                      </button>
                      <button class="tbx-btn" :disabled="transfer.busy" @click="pickImportFolder">
                        选择文件夹…
                      </button>
                    </template>
                    <input
                      v-else v-model="transfer.path" class="tbx-input dk-transfer-path"
                      placeholder="远端 tar 路径，如 /tmp/image.tar"
                      :disabled="transfer.busy" autocomplete="off" spellcheck="false"
                    />
                    <input
                      v-model="transfer.tag" class="tbx-input dk-transfer-tag"
                      :placeholder="transfer.mode === 'local' && transfer.localFiles.length > 1
                        ? '批量导入不支持自定义标签'
                        : '自定义标签（可选，完整引用），如 nginx:my-tag'"
                      :disabled="tagDisabled" autocomplete="off" spellcheck="false"
                    />
                  </template>

                  <span v-if="transfer.busy && transfer.stage" class="dk-transfer-stage">
                    {{ transfer.stage }}{{ transfer.pct > 0 ? ` ${Math.round(transfer.pct)}%` : '' }}
                  </span>
                  <button class="tbx-btn primary" :disabled="transfer.busy" @click="submitTransfer">
                    {{ transfer.busy
                      ? '处理中…'
                      : (transfer.kind === 'export' ? '导出' : transfer.kind === 'import' ? '导入' : '拉取') }}
                  </button>
                  <button class="tbx-link" :disabled="transfer.busy" @click="closeTransfer">取消</button>
                </div>
                <div class="dk-head dk-cols-image" :style="dockerGridStyle('images')">
                  <span
                    v-for="(col, i) in DOCKER_COLUMNS.images" :key="col.label"
                    class="dk-th"
                    :class="{ sortable: !!col.key, sorted: !!col.key && dockerSort.images.key === col.key }"
                    @click="col.key && toggleDockerSort('images', col.key)"
                  >
                    {{ col.label }}
                    <span v-if="col.key && dockerSort.images.key === col.key" class="dk-sort">
                      {{ dockerSort.images.dir === 'asc' ? '▲' : '▼' }}
                    </span>
                    <span
                      v-if="i < DOCKER_COLUMNS.images.length - 1" class="dk-resizer"
                      @click.stop @pointerdown.stop.prevent="startColResize('images', $event, i)"
                    ></span>
                  </span>
                </div>
                <div class="dk-list">
                  <div v-if="busy === 'docker'" class="tbx-empty">正在读取 Docker 容器、镜像、网络与数据卷…</div>
                  <div v-else-if="!docker" class="tbx-empty">尚未读取</div>
                  <div v-else-if="!filteredImages.length" class="tbx-empty">
                    {{ docker.images.length ? '没有匹配筛选条件的镜像' : '未发现镜像' }}
                  </div>
                  <div
                    v-for="img in sortedImages" :key="imageRef(img)" class="dk-row dk-cols-image"
                    :style="dockerGridStyle('images')"
                  >
                    <span class="ellipsis mono" :title="imageRef(img)">{{ imageRef(img) }}</span>
                    <span class="mono muted" :title="img.id">{{ shortId(img.id) }}</span>
                    <span>{{ img.size }}</span>
                    <span class="ellipsis" :title="img.created">{{ img.created }}</span>
                    <span class="dk-ops">
                      <button class="tbx-link" :disabled="!!busy || !!transfer?.busy" @click="startExport(img)">导出</button>
                      <button class="tbx-link danger" :disabled="!!busy" @click="removeImage(img)">删除</button>
                    </span>
                  </div>
                </div>
              </template>

              <!-- 网络列表 -->
              <template v-else-if="dockerView === 'networks'">
                <div v-if="netCreate" class="dk-transfer">
                  <span class="dk-transfer-title">新建网络</span>
                  <input
                    v-model="netCreate.name" class="tbx-input dk-transfer-path"
                    placeholder="网络名称，如 app-net" :disabled="busy === 'docker-act'"
                    autocomplete="off" spellcheck="false"
                  />
                  <select
                    v-model="netCreate.driver" class="tbx-input tbx-select dk-transfer-mode"
                    :disabled="busy === 'docker-act'" title="网络驱动"
                  >
                    <option v-for="d in NETWORK_DRIVERS" :key="d" :value="d">{{ d }}</option>
                  </select>
                  <input
                    v-model="netCreate.subnet" class="tbx-input dk-transfer-path"
                    placeholder="子网（可选）172.20.0.0/16" :disabled="busy === 'docker-act'"
                    autocomplete="off" spellcheck="false"
                  />
                  <input
                    v-model="netCreate.gateway" class="tbx-input dk-transfer-path"
                    placeholder="网关（可选）172.20.0.1" :disabled="busy === 'docker-act'"
                    autocomplete="off" spellcheck="false"
                  />
                  <button
                    class="tbx-btn primary" :disabled="busy === 'docker-act' || !netCreate.name.trim()"
                    @click="createNetwork"
                  >{{ busy === 'docker-act' ? '处理中…' : '创建' }}</button>
                  <button class="tbx-link" :disabled="busy === 'docker-act'" @click="netCreate = null">取消</button>
                </div>
                <div class="dk-head dk-cols-network" :style="dockerGridStyle('networks')">
                  <span
                    v-for="(col, i) in DOCKER_COLUMNS.networks" :key="col.label"
                    class="dk-th"
                    :class="{ sortable: !!col.key, sorted: !!col.key && dockerSort.networks.key === col.key }"
                    @click="col.key && toggleDockerSort('networks', col.key)"
                  >
                    {{ col.label }}
                    <span v-if="col.key && dockerSort.networks.key === col.key" class="dk-sort">
                      {{ dockerSort.networks.dir === 'asc' ? '▲' : '▼' }}
                    </span>
                    <span
                      v-if="i < DOCKER_COLUMNS.networks.length - 1" class="dk-resizer"
                      @click.stop @pointerdown.stop.prevent="startColResize('networks', $event, i)"
                    ></span>
                  </span>
                </div>
                <div class="dk-list">
                  <div v-if="busy === 'docker'" class="tbx-empty">正在读取 Docker 网络…</div>
                  <div v-else-if="!docker" class="tbx-empty">尚未读取</div>
                  <div v-else-if="!filteredNetworks.length" class="tbx-empty">
                    {{ docker.networks.length ? '没有匹配筛选条件的网络' : '未发现网络' }}
                  </div>
                  <div
                    v-for="n in sortedNetworks" :key="n.id || n.name" class="dk-row dk-cols-network"
                    :style="dockerGridStyle('networks')"
                  >
                    <span class="dk-name">
                      <span class="ellipsis" :title="n.name">{{ n.name }}</span>
                      <span class="dk-id mono" :title="n.id">{{ shortId(n.id) }}</span>
                    </span>
                    <span class="mono">{{ n.driver }}</span>
                    <span class="ellipsis mono" :title="[n.subnet, n.gateway].filter(Boolean).join(' · ')">
                      {{ n.subnet || '—' }}<template v-if="n.gateway"> · {{ n.gateway }}</template>
                    </span>
                    <span class="ellipsis" :title="n.containers.join('\n') || '无已连接容器'">
                      {{ networkContainersLabel(n) }}
                    </span>
                    <span class="dk-net-flags">
                      <span class="dk-pill">{{ n.scope || '—' }}</span>
                      <span v-if="n.internal" class="dk-pill warn">内部</span>
                      <span v-if="n.ipv6" class="dk-pill">IPv6</span>
                    </span>
                    <span class="dk-ops">
                      <button
                        class="tbx-link danger" :disabled="!!busy || isBuiltinNetwork(n)"
                        :title="isBuiltinNetwork(n) ? 'Docker 预置网络不可删除' : '删除网络'"
                        @click="removeNetwork(n)"
                      >删除</button>
                    </span>
                  </div>
                </div>
              </template>

              <!-- 数据卷列表 -->
              <template v-else-if="dockerView === 'volumes'">
                <div v-if="volCreate" class="dk-transfer">
                  <span class="dk-transfer-title">新建数据卷</span>
                  <input
                    v-model="volCreate.name" class="tbx-input dk-transfer-path"
                    placeholder="数据卷名称，如 app-data" :disabled="busy === 'docker-act'"
                    autocomplete="off" spellcheck="false"
                  />
                  <input
                    v-model="volCreate.driver" class="tbx-input dk-vol-driver"
                    placeholder="驱动（默认 local）" title="卷驱动，留空使用 local"
                    :disabled="busy === 'docker-act'" autocomplete="off" spellcheck="false"
                  />
                  <button
                    class="tbx-btn primary" :disabled="busy === 'docker-act' || !volCreate.name.trim()"
                    @click="createVolume"
                  >{{ busy === 'docker-act' ? '处理中…' : '创建' }}</button>
                  <button class="tbx-link" :disabled="busy === 'docker-act'" @click="volCreate = null">取消</button>
                </div>
                <div class="dk-head dk-cols-volume" :style="dockerGridStyle('volumes')">
                  <span
                    v-for="(col, i) in DOCKER_COLUMNS.volumes" :key="col.label"
                    class="dk-th"
                    :class="{ sortable: !!col.key, sorted: !!col.key && dockerSort.volumes.key === col.key }"
                    @click="col.key && toggleDockerSort('volumes', col.key)"
                  >
                    {{ col.label }}
                    <span v-if="col.key && dockerSort.volumes.key === col.key" class="dk-sort">
                      {{ dockerSort.volumes.dir === 'asc' ? '▲' : '▼' }}
                    </span>
                    <span
                      v-if="i < DOCKER_COLUMNS.volumes.length - 1" class="dk-resizer"
                      @click.stop @pointerdown.stop.prevent="startColResize('volumes', $event, i)"
                    ></span>
                  </span>
                </div>
                <div class="dk-list">
                  <div v-if="busy === 'docker'" class="tbx-empty">正在读取 Docker 数据卷…</div>
                  <div v-else-if="!docker" class="tbx-empty">尚未读取</div>
                  <div v-else-if="!filteredVolumes.length" class="tbx-empty">
                    {{ docker.volumes.length ? '没有匹配筛选条件的数据卷' : '未发现数据卷' }}
                  </div>
                  <div
                    v-for="v in sortedVolumes" :key="v.name" class="dk-row dk-cols-volume"
                    :style="dockerGridStyle('volumes')"
                  >
                    <span class="dk-name">
                      <span class="ellipsis" :title="v.name">{{ v.name }}</span>
                      <span v-if="isAnonymousVolume(v)" class="dk-id">匿名卷</span>
                    </span>
                    <span class="mono">{{ v.driver || '—' }}</span>
                    <span class="ellipsis mono muted" :title="v.mountpoint">{{ v.mountpoint || '—' }}</span>
                    <span class="ellipsis" :title="v.createdAt">{{ volumeCreatedLabel(v) }}</span>
                    <span class="dk-ops">
                      <button class="tbx-link danger" :disabled="!!busy" @click="removeVolume(v)">删除</button>
                    </span>
                  </div>
                </div>
              </template>

              <!-- Docker 配置 -->
              <template v-else>
                <div class="dk-config">
                  <section class="dk-card">
                    <h4>
                      数据目录 <code>data-root</code>
                      <span class="dk-hint">Docker 镜像与容器数据的存放位置</span>
                    </h4>
                    <div class="dk-card-row">
                      <input
                        v-model="configDataRoot" class="tbx-input dk-grow mono"
                        placeholder="留空使用默认目录，例如 /home/docker1"
                        :disabled="configBusy" autocomplete="off" spellcheck="false"
                      />
                    </div>
                    <p class="dk-hint">
                      留空表示使用 Docker 默认目录（保存时移除该配置）。已有镜像与容器不会自动迁移，
                      需要先停止 Docker 并手动迁移目录。
                    </p>
                  </section>

                  <section class="dk-card">
                    <h4>
                      镜像加速源 <code>registry-mirrors</code>
                      <span class="dk-hint">{{ configMirrors.length }} / 10</span>
                    </h4>
                    <div v-for="(m, i) in configMirrors" :key="i" class="dk-card-row">
                      <input
                        v-model="configMirrors[i]" class="tbx-input dk-grow"
                        :title="m" placeholder="https://mirror.example.com" :disabled="configBusy"
                        autocomplete="off" spellcheck="false"
                      />
                      <button class="tbx-link danger" :disabled="configBusy" @click="configMirrors.splice(i, 1)">移除</button>
                    </div>
                    <div class="dk-card-row">
                      <button
                        class="tbx-btn" :disabled="configBusy || configMirrors.length >= 10"
                        @click="configMirrors.push('')"
                      >＋ 添加镜像源</button>
                    </div>
                    <p class="dk-hint">清空列表后保存会移除镜像源配置。</p>
                  </section>

                  <section class="dk-card">
                    <h4>
                      其他配置项
                      <span class="dk-hint">{{ configOthers.length }} 项 · 保存时原样保留</span>
                    </h4>
                    <div v-if="!daemonObject" class="tbx-empty">daemon.json 不是合法 JSON，暂不能在此编辑</div>
                    <p v-else-if="!configOthers.length" class="dk-hint">当前没有其他配置项。</p>
                    <div v-for="item in configOthers" :key="item.key" class="dk-other">
                      <code class="dk-other-key">{{ item.key }}</code>
                      <pre class="dk-other-val">{{ item.text }}</pre>
                    </div>
                  </section>

                  <section class="dk-card">
                    <h4>
                      保存预览
                      <span class="dk-hint">{{ dockerConfig?.configPath || '/etc/docker/daemon.json' }}</span>
                    </h4>
                    <textarea
                      class="dk-preview-edit mono"
                      :value="configEditorText"
                      :disabled="configBusy || !dockerConfig"
                      spellcheck="false" autocomplete="off" placeholder="{}"
                      @input="configPreviewEdit = ($event.target as HTMLTextAreaElement).value"
                    ></textarea>
                    <p class="dk-hint">
                      预览可直接编辑，保存时按此处内容写入；之后再改上方表单，会同步更新对应配置项。
                    </p>
                    <div class="dk-card-row">
                      <span class="dk-hint">保存前自动备份原文件，重启失败自动还原。</span>
                      <button
                        v-if="configPreviewEdit !== null" class="tbx-link" :disabled="configBusy"
                        @click="configPreviewEdit = null"
                      >撤销编辑</button>
                      <span class="spacer"></span>
                      <button
                        class="tbx-btn primary" :disabled="configBusy || !dockerConfig"
                        @click="saveDockerConfig"
                      >{{ configBusy ? '处理中…' : '保存并重启 Docker' }}</button>
                    </div>
                  </section>
                </div>
              </template>

              <p v-if="dockerView !== 'config'" class="pt-note">
                容器、镜像与网络通过 Docker socket 读取；普通用户需加入 docker 组或具备 sudo 权限。
                删除容器不会清理数据卷；镜像可从镜像地址拉取、从远端 tar 导入或导出为 tar，导入导出后可再通过 SFTP 下载或上传。
                新建网络可指定驱动与子网/网关；bridge / host / none 为 Docker 预置网络，不可删除。
              </p>
            </template>
          </template>
        </div>

        <div class="modal-foot">
          <span class="tbx-tip">
            需要 root 的操作会按需询问 sudo 密码，密码仅保存在本机内存，断开连接即清除。
          </span>
          <span class="spacer"></span>
          <button class="tbx-btn" @click="close">关闭</button>
        </div>
      </div>
    </div>
  </Teleport>
</template>

<style scoped>
/* 本弹窗 Teleport 到 body，与 #app 内弹窗同为 z-index 60 时会永远压住后者；
   降低一层，保证 sudo 密码框、确认框与 toast 能正常盖在工具箱之上 */
.mask { z-index: 55; }
.tbx-modal {
  width: 900px;
  height: min(680px, calc(100vh - 60px));
}
.tbx-tabs {
  display: flex;
  gap: 4px;
  padding: 10px 16px 0;
  border-bottom: 1px solid var(--border);
  flex-shrink: 0;
}
.tbx-tab {
  padding: 7px 14px;
  border: 1px solid transparent;
  border-bottom: none;
  border-radius: 7px 7px 0 0;
  background: none;
  font: inherit;
  font-size: 12px;
  color: var(--muted);
  cursor: pointer;
}
.tbx-tab:hover { color: var(--text); background: var(--hover); }
.tbx-tab.active {
  color: var(--text-strong);
  background: var(--panel-2);
  border-color: var(--border);
}
.tbx-body {
  flex: 1;
  min-height: 0;
  display: flex;
  flex-direction: column;
  padding: 12px 16px;
  gap: 10px;
}
.tbx-empty {
  padding: 26px 0;
  text-align: center;
  font-size: 12px;
  color: var(--muted-2);
}
.tbx-error {
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
}
.tbx-link {
  border: 0;
  background: none;
  padding: 0;
  font: inherit;
  font-size: 11.5px;
  color: var(--blue);
  cursor: pointer;
}
.tbx-link:hover { text-decoration: underline; }
.tbx-link.danger { color: var(--red); }
.tbx-error-close {
  margin-left: auto;
  align-self: flex-start;
  border: 0;
  background: none;
  padding: 0 2px;
  font: inherit;
  font-size: 14px;
  line-height: 1.1;
  color: var(--red);
  opacity: 0.6;
  cursor: pointer;
}
.tbx-error-close:hover { opacity: 1; }
.tbx-btn {
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
.tbx-btn:hover:not(:disabled) { border-color: var(--border-strong); background: var(--hover); }
.tbx-btn:disabled { opacity: .5; cursor: default; }
.tbx-btn.primary {
  border-color: color-mix(in srgb, var(--green) 45%, transparent);
  color: var(--green);
}
.tbx-btn.danger {
  border-color: color-mix(in srgb, var(--red) 45%, transparent);
  color: var(--red);
}
.tbx-input {
  padding: 6px 9px;
  border: 1px solid var(--border);
  border-radius: 7px;
  background: var(--panel-2);
  font: inherit;
  font-size: 11.5px;
  color: var(--text);
  min-width: 0;
}
.tbx-input:focus { outline: none; border-color: var(--border-strong); }
.tbx-select { cursor: pointer; }
.spacer { flex: 1; }
.mono { font-family: var(--mono, monospace); }
.muted { color: var(--muted-2); }
.ellipsis { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }

/* 防火墙 */
.fw-title {
  display: flex;
  align-items: center;
  flex-shrink: 0;
}
.fw-title h3 {
  margin: 0;
  font-size: 14px;
  font-weight: 600;
  color: var(--text-strong);
}
.fw-status {
  position: relative;
  overflow: hidden;
  display: flex;
  align-items: center;
  gap: 8px;
  flex-shrink: 0;
  padding: 8px 10px;
  border: 1px solid var(--border);
  border-radius: 8px;
  background: var(--panel-2);
}
.fw-pill {
  padding: 2px 9px;
  border-radius: 999px;
  border: 1px solid var(--border);
  background: var(--panel);
  font-size: 10px;
  font-weight: 600;
  letter-spacing: .06em;
  text-transform: uppercase;
  color: var(--muted);
}
.fw-pill.on {
  color: var(--green);
  border-color: color-mix(in srgb, var(--green) 45%, transparent);
}
.fw-status-text { font-size: 11.5px; color: var(--muted); }
.fw-status-text.on { color: var(--green); }
.fw-switch-text { font-size: 11px; color: var(--muted); }
.fw-switch {
  position: relative;
  width: 38px;
  height: 20px;
  padding: 0;
  border: 1px solid var(--border-strong);
  border-radius: 999px;
  background: var(--panel);
  cursor: pointer;
  transition: background .15s, border-color .15s;
}
.fw-switch .fw-knob {
  position: absolute;
  top: 2px;
  left: 2px;
  width: 14px;
  height: 14px;
  border-radius: 50%;
  background: var(--muted);
  transition: transform .15s, background .15s;
}
.fw-switch.on {
  border-color: color-mix(in srgb, var(--green) 55%, transparent);
  background: color-mix(in srgb, var(--green) 22%, transparent);
}
.fw-switch.on .fw-knob { transform: translateX(18px); background: var(--green); }
.fw-switch:disabled { opacity: .45; cursor: default; }
.fw-switch.busy:disabled { opacity: 1; }
.fw-switch.busy .fw-knob { animation: fw-knob-pulse 1.1s ease-in-out infinite; }
.fw-progress {
  position: absolute;
  left: 0;
  right: 0;
  bottom: 0;
  height: 3px;
  background: color-mix(in srgb, var(--green) 16%, transparent);
  overflow: hidden;
}
.fw-progress-bar {
  display: block;
  width: 38%;
  height: 100%;
  border-radius: 999px;
  background: var(--green);
  animation: fw-progress 1.1s ease-in-out infinite;
}
@keyframes fw-progress {
  0% { transform: translateX(-100%); }
  100% { transform: translateX(280%); }
}
@keyframes fw-knob-pulse {
  0%, 100% { opacity: 1; }
  50% { opacity: .35; }
}
.fw-stats {
  display: grid;
  grid-template-columns: repeat(4, minmax(0, 1fr));
  gap: 8px;
  flex-shrink: 0;
}
.fw-stat {
  display: flex;
  flex-direction: column;
  gap: 3px;
  padding: 9px 12px;
  border: 1px solid var(--border);
  border-left-width: 3px;
  border-radius: 8px;
  background: var(--panel-2);
}
.fw-stat-label { font-size: 10.5px; color: var(--muted); }
.fw-stat strong {
  font-size: 18px;
  font-weight: 600;
  line-height: 1.1;
}
.fw-stat.total { border-left-color: var(--blue); }
.fw-stat.total strong { color: var(--blue); }
.fw-stat.allow { border-left-color: var(--green); }
.fw-stat.allow strong { color: var(--green); }
.fw-stat.deny { border-left-color: var(--red); }
.fw-stat.deny strong { color: var(--red); }
.fw-stat.other { border-left-color: var(--yellow); }
.fw-stat.other strong { color: var(--yellow); }
.fw-filter {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-shrink: 0;
}
.fw-filter .tbx-input:first-child { flex: 1; }
.fw-note, .pt-note {
  margin: 0;
  font-size: 10.5px;
  color: var(--muted-2);
  flex-shrink: 0;
}
.fw-form {
  display: grid;
  grid-template-columns: 92px minmax(140px, 1fr) 100px minmax(140px, 1fr) auto;
  gap: 8px;
  flex-shrink: 0;
}
.fw-head, .fw-row {
  display: grid;
  grid-template-columns: 58px 100px 64px 150px minmax(0, 1fr) 60px;
  gap: 8px;
  align-items: center;
  padding: 0 8px;
}
.fw-head {
  font-size: 10.5px;
  font-weight: 600;
  color: var(--muted);
  padding-top: 6px;
  padding-bottom: 6px;
  border-bottom: 1px solid var(--border);
  flex-shrink: 0;
}
.fw-list { flex: 1; min-height: 0; overflow-y: auto; }
.fw-row {
  padding-top: 5px;
  padding-bottom: 5px;
  font-size: 11.5px;
  color: var(--text);
  border-bottom: 1px solid color-mix(in srgb, var(--border) 55%, transparent);
}
.fw-row:hover { background: var(--panel-2); }
.grow { min-width: 0; }
.fw-action { font-size: 11px; }
.fw-action.allow { color: var(--green); }
.fw-action.deny { color: var(--red); }
.fw-action.other { color: var(--muted); }

/* 端口 */
.pt-bar {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-shrink: 0;
}
.pt-bar .tbx-input:first-child { flex: 1; }
.pt-tool { font-size: 10.5px; color: var(--muted-2); white-space: nowrap; }
.pt-count { font-size: 10.5px; color: var(--muted); white-space: nowrap; }
.pt-head, .pt-row {
  display: grid;
  grid-template-columns: 70px 52px minmax(0, 1.1fr) minmax(0, 1fr) 90px minmax(0, 1fr) 60px;
  gap: 8px;
  align-items: center;
  padding: 0 8px;
}
.pt-head {
  font-size: 10.5px;
  font-weight: 600;
  color: var(--muted);
  padding-top: 6px;
  padding-bottom: 6px;
  border-bottom: 1px solid var(--border);
  flex-shrink: 0;
}
.pt-list { flex: 1; min-height: 0; overflow-y: auto; }
.pt-row {
  padding-top: 5px;
  padding-bottom: 5px;
  font-size: 11.5px;
  color: var(--text);
  border-bottom: 1px solid color-mix(in srgb, var(--border) 55%, transparent);
}
.pt-row:hover { background: var(--panel-2); }
.pt-container { color: var(--blue); }
.pt-copyable { cursor: pointer; }
.pt-copyable:hover { text-decoration: underline; }

/* 公钥 */
.pk-target-preview {
  max-width: 380px;
  font-size: 10.5px;
  color: var(--muted);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.pk-form {
  display: flex;
  flex-direction: column;
  gap: 10px;
  min-height: 0;
  overflow-y: auto;
}
.pk-row {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-shrink: 0;
}
.pk-label {
  width: 56px;
  flex-shrink: 0;
  font-size: 11.5px;
  color: var(--muted);
}
.pk-key { flex: 1; }
.pk-user { width: 200px; }
.pk-meta {
  display: flex;
  flex-direction: column;
  gap: 4px;
  padding: 8px 10px;
  border: 1px solid var(--border);
  border-radius: 8px;
  background: var(--panel-2);
  font-size: 11px;
}
.pk-meta-sub { font-size: 10.5px; color: var(--muted-2); }
.pk-textarea {
  min-height: 104px;
  resize: vertical;
  font-family: var(--mono, monospace);
  font-size: 11px;
  line-height: 1.55;
}
.pk-hint {
  margin: 0;
  font-size: 11px;
  line-height: 1.65;
  color: var(--muted-2);
}
.pk-hint code {
  font-family: var(--mono, monospace);
  font-size: 10.5px;
  color: var(--muted);
}
.pk-result {
  display: flex;
  flex-direction: column;
  gap: 4px;
  padding: 8px 10px;
  border: 1px solid color-mix(in srgb, var(--green) 40%, transparent);
  border-radius: 8px;
  background: color-mix(in srgb, var(--green) 10%, transparent);
  font-size: 11.5px;
  color: var(--green);
  word-break: break-all;
}
.pk-result.exists {
  border-color: var(--border);
  background: var(--panel-2);
  color: var(--muted);
}
.pk-result code { font-family: var(--mono, monospace); font-size: 11px; }

/* Docker */
.dk-bar {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: 8px;
  flex-shrink: 0;
}
.dk-search { flex: 1 1 200px; min-width: 200px; }
.dk-seg {
  display: flex;
  flex-shrink: 0;
  border: 1px solid var(--border);
  border-radius: 7px;
  overflow: hidden;
}
.dk-seg button {
  display: flex;
  align-items: center;
  gap: 5px;
  padding: 6px 11px;
  border: 0;
  background: var(--panel-2);
  font: inherit;
  font-size: 11.5px;
  color: var(--muted);
  cursor: pointer;
}
.dk-seg button + button { border-left: 1px solid var(--border); }
.dk-seg button:hover { color: var(--text); background: var(--hover); }
.dk-seg button.active { color: var(--text-strong); background: var(--hover); }
.dk-seg-count {
  padding: 0 6px;
  border-radius: 999px;
  background: var(--panel);
  font-size: 10px;
  color: var(--muted);
}
.dk-head, .dk-row {
  display: grid;
  gap: 8px;
  align-items: center;
  padding: 0 8px;
}
.dk-cols-container {
  grid-template-columns: minmax(0, 1.2fr) minmax(0, 1.1fr) 140px minmax(0, 1fr) 110px 170px;
}
.dk-cols-image {
  grid-template-columns: minmax(0, 1.6fr) 110px 76px 110px 76px;
}
.dk-cols-network {
  grid-template-columns: minmax(0, 1.1fr) 58px minmax(0, 1.35fr) minmax(0, 1.35fr) 138px 56px;
}
.dk-cols-volume {
  grid-template-columns: minmax(0, 1.15fr) 64px minmax(0, 2fr) 128px 56px;
}
.dk-vol-driver { flex: 0 0 170px; }
.dk-net-flags {
  display: flex;
  align-items: center;
  gap: 4px;
  min-width: 0;
  flex-wrap: nowrap;
}
.dk-pill {
  padding: 0 6px;
  border: 1px solid var(--border);
  border-radius: 999px;
  background: var(--panel-2);
  font-size: 9.5px;
  line-height: 16px;
  color: var(--muted);
  white-space: nowrap;
}
.dk-pill.warn {
  border-color: color-mix(in srgb, var(--yellow) 45%, transparent);
  color: var(--yellow);
}
.dk-head {
  font-size: 10.5px;
  font-weight: 600;
  color: var(--muted);
  padding-top: 6px;
  padding-bottom: 6px;
  border-bottom: 1px solid var(--border);
  flex-shrink: 0;
}
.dk-th {
  position: relative;
  display: flex;
  align-items: center;
  min-width: 0;
  white-space: nowrap;
}
.dk-th.sortable { cursor: pointer; }
.dk-th.sortable:hover { color: var(--text); }
.dk-th.sorted { color: var(--text); }
.dk-sort {
  margin-left: 3px;
  font-size: 8px;
  line-height: 1;
  color: var(--blue);
}
.dk-resizer {
  position: absolute;
  top: -6px;
  bottom: -6px;
  right: -5px;
  width: 10px;
  z-index: 2;
  cursor: col-resize;
}
.dk-resizer::after {
  content: '';
  position: absolute;
  top: 7px;
  bottom: 7px;
  left: 4px;
  width: 1px;
  background: var(--border);
  opacity: 0;
}
.dk-th:hover .dk-resizer::after { opacity: 1; }
.dk-list { flex: 1; min-height: 0; overflow-y: auto; }
.dk-row {
  padding-top: 5px;
  padding-bottom: 5px;
  font-size: 11.5px;
  color: var(--text);
  border-bottom: 1px solid color-mix(in srgb, var(--border) 55%, transparent);
}
.dk-row:hover { background: var(--panel-2); }
.dk-name {
  display: flex;
  flex-direction: column;
  gap: 1px;
  min-width: 0;
}
.dk-id { font-size: 9.5px; color: var(--muted-2); }
.dk-state {
  display: flex;
  flex-direction: column;
  gap: 1px;
  min-width: 0;
  font-size: 11px;
}
.dk-state.running { color: var(--green); }
.dk-state.paused, .dk-state.restarting { color: var(--yellow); }
.dk-state.exited, .dk-state.dead { color: var(--muted); }
.dk-state.removing { color: var(--red); }
.dk-status { font-size: 9.5px; color: var(--muted-2); }
.dk-ops { display: flex; align-items: center; gap: 7px; flex-wrap: nowrap; }
.tbx-link:disabled { opacity: .45; cursor: default; }
.tbx-tip { font-size: 10.5px; color: var(--muted-2); }

/* Docker 浮层（日志 / 终端） */
.dk-panel-head {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-shrink: 0;
  min-width: 0;
}
.dk-panel-title {
  max-width: 260px;
  font-size: 12px;
  color: var(--text-strong);
}
.dk-check {
  display: flex;
  align-items: center;
  gap: 5px;
  font-size: 11px;
  color: var(--muted);
  white-space: nowrap;
  cursor: pointer;
}
.dk-logs-wrap {
  flex: 1;
  min-height: 0;
  display: flex;
  flex-direction: column;
  border: 1px solid var(--border);
  border-radius: 8px;
  background: var(--panel-2);
  overflow: hidden;
}
.dk-logs {
  flex: 1;
  min-height: 0;
  margin: 0;
  padding: 10px 12px;
  overflow: auto;
  font-family: var(--mono, monospace);
  font-size: 11px;
  line-height: 1.55;
  color: var(--text);
  white-space: pre;
  tab-size: 4;
}
.dk-term {
  flex: 1;
  min-height: 0;
  border: 1px solid var(--border);
  border-radius: 8px;
  overflow: hidden;
  background: var(--panel-2);
}

/* Docker 镜像导入 / 导出 */
.dk-transfer {
  display: flex;
  align-items: center;
  flex-wrap: wrap;
  gap: 8px;
  flex-shrink: 0;
  padding: 8px 10px;
  border: 1px solid var(--border);
  border-radius: 8px;
  background: var(--panel-2);
}
.dk-transfer-title { font-size: 11.5px; color: var(--text-strong); white-space: nowrap; }
.dk-transfer-img { max-width: 190px; font-size: 10.5px; color: var(--muted); }
.dk-transfer-path { flex: 1 1 180px; min-width: 150px; }
.dk-transfer-tag { flex: 0 1 200px; min-width: 130px; }
.dk-transfer-mode { flex: 0 0 100px; }
.dk-transfer-stage { font-size: 11px; color: var(--muted); white-space: nowrap; }

/* Docker 配置 */
.dk-config {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  display: flex;
  flex-direction: column;
  gap: 10px;
}
.dk-card {
  display: flex;
  flex-direction: column;
  gap: 8px;
  padding: 12px;
  border: 1px solid var(--border);
  border-radius: 8px;
  background: var(--panel-2);
  flex-shrink: 0;
}
.dk-card h4 {
  margin: 0;
  display: flex;
  align-items: baseline;
  gap: 8px;
  font-size: 12px;
  font-weight: 600;
  color: var(--text-strong);
}
.dk-card-row { display: flex; align-items: center; gap: 8px; }
.dk-grow { flex: 1; }
.dk-hint {
  margin: 0;
  font-size: 10.5px;
  line-height: 1.6;
  color: var(--muted-2);
}
.dk-hint code { font-family: var(--mono, monospace); font-size: 10.5px; color: var(--muted); }
.dk-card h4 code {
  font-family: var(--mono, monospace);
  font-size: 10.5px;
  font-weight: 500;
  color: var(--muted);
}
.dk-other { display: flex; flex-direction: column; gap: 2px; }
.dk-other-key { font-family: var(--mono, monospace); font-size: 10.5px; color: var(--muted); }
.dk-other-val {
  margin: 0;
  padding: 6px 8px;
  border: 1px solid var(--border);
  border-radius: 6px;
  background: var(--panel);
  font-family: var(--mono, monospace);
  font-size: 11px;
  line-height: 1.5;
  color: var(--text);
  white-space: pre-wrap;
  word-break: break-all;
  overflow: auto;
}
.dk-other-val { max-height: 120px; }
.dk-preview-edit {
  box-sizing: border-box;
  width: 100%;
  min-height: 180px;
  max-height: 320px;
  padding: 6px 8px;
  border: 1px solid var(--border);
  border-radius: 6px;
  background: var(--panel);
  font-family: var(--mono, monospace);
  font-size: 11px;
  line-height: 1.5;
  color: var(--text);
  resize: vertical;
  overflow: auto;
  white-space: pre;
}
.dk-preview-edit:focus { outline: none; border-color: var(--border-strong); }
.dk-preview-edit:disabled { opacity: .6; }
</style>
