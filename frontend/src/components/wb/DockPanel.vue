<script setup lang="ts">
import { computed, nextTick, onMounted, onUnmounted, ref, watch } from 'vue'
import { invoke, Channel } from '@tauri-apps/api/core'
import { tempDir } from '@tauri-apps/api/path'
import { open } from '@tauri-apps/plugin-dialog'
import { revealItemInDir } from '@tauri-apps/plugin-opener'
import { activeHost, activeSession, activeSftpCwd, closeDock, connected, dockCollapsed, dockHeight, DOCK_DEFAULT_HEIGHT, dockTab, dockVisible, searchTick, sessions, sftpLocalRatio, sftpTreeWidth } from '../../stores/session'
import { savedSettings } from '../../stores/settings'
import { toast } from '../../composables/useToast'
import { confirmDialog } from '../../composables/useConfirm'
import TunnelPane from './TunnelPane.vue'
import PermModal from './PermModal.vue'
import { detectPlatform, isTauri } from '../../lib/tauri'
import { dropToCss, watchFileDrop } from '../../lib/fileDrop'
import { getSnapshot, onConfigLoad, patchUiState } from '../../stores/appConfig'
import ContextMenu from './ContextMenu.vue'
import type { MenuItem } from './ContextMenu.vue'
import {
  logs, subscribe as subscribeAppLogs, clear as clearAppLogs, revealLogDir,
  report as reportLog,
  SFTP_TRANSFER_ENQUEUE, SFTP_TRANSFER_START, SFTP_TRANSFER_COMPLETE,
  SFTP_TRANSFER_FAILED, SFTP_TRANSFER_CANCEL, SFTP_TRANSFER_PAUSE,
  SFTP_TRANSFER_RESUME, SFTP_TRANSFER_BLOCKED, SFTP_TRANSFER_RETRY, SFTP_MANAGE_REMOVE,
  SFTP_DROP_ACCEPTED, SFTP_DROP_MISSED,
  SFTP_MANAGE_CHMOD,
  SFTP_EDIT_OPEN, SFTP_EDIT_SYNC, SFTP_EDIT_CONFLICT,
} from '../../stores/applog'
import type { LogLevel } from '../../stores/applog'
import {
  markTransferRunning, runningOnBackend, runningTotal, clearTransferRunning,
} from '../../lib/transferLedger'

type Side = 'local' | 'remote'

/* ---- SFTP 文件系统（本地/远程均为后端真实数据） ---- */
interface FsEntry {
  name: string
  type: 'dir' | 'file'
  size: number
  mtime: string
  perm: string      // e.g. "rwxr-xr-x"
  ownerUser: string // e.g. "root"
  ownerGroup: string// e.g. "root"
  kind: string      // ls -l 类型位：d/l/-/c/b/p/s，空串=后端未给出
  isSymlink: boolean
}

/* 后端 list_local_dir / sftp_list_dir 返回结构（camelCase，两侧同构） */
interface DirListing {
  path: string
  entries: Array<{
    name: string
    isDir: boolean
    isSymlink: boolean
    size: number
    mtime: string
    perm: string
    ownerUser: string
    ownerGroup: string
    kind: string
  }>
}

type SortKey = 'name' | 'size' | 'mtime'
interface SortState { key: SortKey; dir: 1 | -1 }

const localPath = ref('/')
const remotePath = ref('/')
/** 本机主目录（首次 list_local_dir 回填）：仅用于把下载目录显示成 ~ 短形式 */
const localHome = ref('')
/** 本机文件栏开关（设置 → SFTP → 显示本机文件栏）。默认关闭：SFTP 只留远端一栏，
 *  上传走文件区拖拽，下载目录由顶栏「下载到」选择；localPath 始终是可改的下载目标 */
const showLocalPane = computed(() => savedSettings.sftpShowLocalPane)

/* ---- 行内编辑（新建文件夹/新建文件/重命名统一一个状态，不弹窗） ---- */
type EditKind = 'mkdir' | 'mkfile' | 'rename'
interface EditState { side: Side; kind: EditKind; origName?: string }
const edit = ref<EditState | null>(null)
const editName = ref('')
const editInput = ref<HTMLInputElement | null>(null)

/** 开始编辑：新建类名空；重命名回填原名并全选 */
function startEdit(side: Side, kind: EditKind, target?: FsEntry) {
  if (kind === 'rename' && target) {
    edit.value = { side, kind, origName: target.name }
    editName.value = target.name
  } else {
    edit.value = { side, kind }
    editName.value = ''
  }
  nextTick(() => {
    editInput.value?.focus()
    if (kind === 'rename') editInput.value?.select()
  })
}

/** 确认编辑：校验名称/重名 → 调后端 → 刷新；空名/未改名直接收起 */
async function confirmEdit() {
  const st = edit.value
  if (!st) return
  const name = editName.value.trim()
  edit.value = null
  if (!name) return
  if (st.kind === 'rename' && name === st.origName) return
  if (name.includes('/')) {
    toast('名称不能包含 /', 'warn')
    return
  }
  if (name === '.' || name === '..') {
    toast('名称不能为 . 或 ..', 'warn')
    return
  }
  if (st.kind !== 'rename' && entryExists(st.side, name)) {
    toast('已存在同名项', 'warn')
    return
  }
  const base = sidePath(st.side)
  try {
    if (st.kind === 'rename') {
      const oldPath = joinPath(base, st.origName!)
      const newPath = joinPath(base, name)
      await invokeSide(st.side, 'rename', { oldPath, newPath })
    } else if (st.kind === 'mkdir') {
      await invokeSide(st.side, 'mkdir', { path: joinPath(base, name) })
    } else {
      await invokeSide(st.side, 'create_file', { path: joinPath(base, name) })
    }
    void refreshSide(st.side)
  } catch (e) {
    const verb = st.kind === 'rename' ? '重命名' : st.kind === 'mkdir' ? '新建文件夹' : '新建文件'
    toast(`${verb}失败：${(e as Error).message}`, 'warn')
  }
}

/** Esc 取消（edit 先置空，随后的 blur 即 no-op） */
function cancelEdit() {
  edit.value = null
}

/* ---- 文件剪贴板（复制/剪切；记录复制瞬间的源目录绝对路径与元数据，允许随后导航） ---- */
const clipboard = ref<{
  srcSide: Side
  base: string
  mode: 'copy' | 'cut'
  items: { name: string; isDir: boolean; size: number }[]
} | null>(null)

/* ---- 右键菜单状态：事件入口一次性写全坐标与目标，菜单项由 computed 生成 ---- */
const menu = ref<{ x: number; y: number; side: Side; targets: FsEntry[] } | null>(null)

/* 最后交互的一侧：快捷键作用域（点击/右键/导航时更新） */
const lastSide = ref<Side>('local')

const localSelected = ref<Set<string>>(new Set())
const remoteSelected = ref<Set<string>>(new Set())
const localSort = ref<SortState>({ key: 'name', dir: 1 })
const remoteSort = ref<SortState>({ key: 'name', dir: 1 })

/* 本地目录真实数据：由后端 list_local_dir 加载；localPath 始终对应已加载成功的目录 */
const localRaw = ref<FsEntry[]>([])
const localLoading = ref(false)

async function loadLocal(path?: string) {
  if (!isTauri) return // 浏览器 dev 模式无本地 FS，保持空列表
  localLoading.value = true
  try {
    const res = await invoke<DirListing>('list_local_dir', { path: path ?? null })
    localRaw.value = res.entries.map(toFsEntry)
    localPath.value = res.path
    if (!localHome.value) localHome.value = res.path
    localSelected.value = new Set()
  } catch (e) {
    toast(String(e), 'warn', 2200)
  } finally {
    localLoading.value = false
  }
}

/* 远端目录真实数据：由后端 sftp_list_dir 经会话 SFTP 子系统加载。
   remotePath 始终对应已加载成功的目录；请求失败时停留在原目录。 */
const remoteRaw = ref<FsEntry[]>([])
const remoteLoading = ref(false)
let remoteReqSeq = 0 // 竞态守卫：快速切目录/切会话时丢弃过期响应

async function loadRemote(path?: string) {
  const backendId = activeSession.value?.backendId
  if (!isTauri || !backendId) return // 浏览器 dev 模式 / 未连接：保持空列表
  const req = ++remoteReqSeq
  remoteLoading.value = true
  try {
    const res = await invoke<DirListing>('sftp_list_dir', {
      sessionId: backendId,
      path: path ?? null,
    })
    // 过期响应（用户已继续导航或切换/断开会话）直接丢弃，避免覆盖新视图
    if (req !== remoteReqSeq || activeSession.value?.backendId !== backendId) return
    remoteRaw.value = res.entries.map(toFsEntry)
    remotePath.value = res.path
    remoteSelected.value = new Set()
    // 同一份列表顺手喂给目录树：这一层刚列过，不必再发一次 SFTP 请求
    treeChildren.value[res.path] = treeKidsOf(remoteRaw.value, res.path)
    treeFailed.value[res.path] = false
    pushRemoteHistory(res.path)
  } catch (e) {
    if (req !== remoteReqSeq) return
    toast(String(e), 'warn', 2600)
  } finally {
    if (req === remoteReqSeq) remoteLoading.value = false
  }
}

/** 后端目录项 → 前端通用 FsEntry（本地/远程共用） */
function toFsEntry(e: DirListing['entries'][number]): FsEntry {
  return {
    name: e.name,
    type: e.isDir ? 'dir' : 'file',
    size: e.size,
    mtime: e.mtime,
    perm: e.perm,
    ownerUser: e.ownerUser,
    ownerGroup: e.ownerGroup,
    kind: e.kind,
    isSymlink: e.isSymlink,
  }
}

/** 「类型」列文案：以后端给出的 ls -l 类型位为准，缺失时按 isDir 兜底
 *  （部分 sshd 的 NAME 响应不带 longname，或格式非标准） */
const KIND_LABEL: Record<string, string> = {
  d: '文件夹', l: '链接', c: '字符设备', b: '块设备', p: '管道', s: '套接字', '-': '文件',
}
function kindLabel(e: FsEntry): string {
  const v = KIND_LABEL[e.kind]
  if (v) return v
  return e.type === 'dir' ? '文件夹' : '文件'
}

function joinPath(dir: string, name: string) {
  if (dir === '/') return '/' + name
  return dir + '/' + name
}
function parentPath(path: string) {
  if (path === '/') return '/'
  const i = path.lastIndexOf('/')
  if (i === 0) return '/'
  return path.slice(0, i)
}

/* ---- 远程目录树（「文件」页左栏，FinalShell 形态） ----
   懒加载：只列展开过的目录，且只留目录项；每个目录的子项来源有两条——点三角
   或经过文件列表导航，后者直接复用已经拉到的那份列表，不再多发一次 SFTP 请求。
   状态是 path 作键的几张表，渲染前 DFS 压平成可见行，因此不需要递归组件。 */
interface TreeDir { name: string; path: string }
interface TreeRow extends TreeDir { depth: number; expanded: boolean; loading: boolean; failed: boolean }

const TREE_ROOT = '/'
/** path → 该目录下的子目录（undefined = 还没列过） */
const treeChildren = ref<Record<string, TreeDir[]>>({})
const treeExpanded = ref<Record<string, boolean>>({})
const treeLoading = ref<Record<string, boolean>>({})
const treeFailed = ref<Record<string, boolean>>({})

/** 目录列表 → 树的子节点：与文件列表同一套隐藏规则，避免树里点得到的目录列表里没有 */
function treeKidsOf(list: FsEntry[], dir: string): TreeDir[] {
  return visibleEntries(list)
    .filter(e => e.type === 'dir')
    .map(e => ({ name: e.name, path: joinPath(dir, e.name) }))
    .sort((a, b) => a.name.localeCompare(b.name))
}

async function loadTreeChildren(path: string) {
  const backendId = activeSession.value?.backendId
  if (!isTauri || !backendId) return
  if (treeChildren.value[path] !== undefined || treeLoading.value[path]) return
  treeLoading.value[path] = true
  treeFailed.value[path] = false
  try {
    const res = await invoke<DirListing>('sftp_list_dir', { sessionId: backendId, path })
    if (activeSession.value?.backendId !== backendId) return // 换会话/断开：丢弃过期响应
    treeChildren.value[res.path] = treeKidsOf(res.entries.map(toFsEntry), res.path)
  } catch {
    // 无权限/已删除：留一个可重试的失败标记，不写 children，下次点击会再试
    treeFailed.value[path] = true
  } finally {
    treeLoading.value[path] = false
  }
}

function toggleTree(path: string) {
  if (treeExpanded.value[path]) {
    treeExpanded.value[path] = false
    return
  }
  treeExpanded.value[path] = true
  void loadTreeChildren(path)
}

/** 点行：右侧列表切到该目录，同时展开它，使树与列表停在同一处 */
function onTreeRowClick(r: TreeRow) {
  if (!r.expanded) toggleTree(r.path)
  if (r.path !== remotePath.value) gotoCrumb('remote', r.path)
}

const treeRows = computed<TreeRow[]>(() => {
  const out: TreeRow[] = []
  const walk = (node: TreeDir, depth: number) => {
    const expanded = !!treeExpanded.value[node.path]
    out.push({
      ...node, depth, expanded,
      loading: !!treeLoading.value[node.path],
      failed: !!treeFailed.value[node.path],
    })
    if (!expanded) return
    treeChildren.value[node.path]?.forEach(k => walk(k, depth + 1))
  }
  // 根永远是一行：未列出时它也是可点的重试入口
  walk({ name: TREE_ROOT, path: TREE_ROOT }, 0)
  return out
})

/** 会话建立/切换：清掉上一台主机的树，重新列根目录
    （ssh:// 外部连接不列目录：那会另开一条 SFTP 通道，后端也会拒绝） */
function resetTree() {
  treeChildren.value = {}
  treeExpanded.value = {}
  treeLoading.value = {}
  treeFailed.value = {}
  revealSeq++
  remoteHistory.value = []
  historyOpen.value = false
  if (!activeSession.value?.backendId || extTerminal.value) return
  treeExpanded.value[TREE_ROOT] = true
  void loadTreeChildren(TREE_ROOT)
}

/** 文件列表换目录后把树上那条路径摊开；seq 守卫避免连续 cd 时交错 */
let revealSeq = 0
async function revealInTree(path: string) {
  const chain: string[] = [TREE_ROOT]
  if (path !== TREE_ROOT) {
    let acc = ''
    for (const seg of path.split('/').filter(Boolean)) {
      acc += '/' + seg
      chain.push(acc)
    }
  }
  const my = ++revealSeq
  for (const p of chain) {
    treeExpanded.value[p] = true
    await loadTreeChildren(p)
    if (my !== revealSeq) return // 期间又导航了一次，让最新的那次接管
  }
}

/** 树工具条的刷新：清掉缓存，把仍处于展开状态的层重新列一遍（折叠的不发请求） */
function reloadTree() {
  if (!activeSession.value?.backendId) return
  const open = Object.keys(treeChildren.value).filter(p => treeExpanded.value[p])
  treeChildren.value = {}
  treeFailed.value = {}
  void loadTreeChildren(TREE_ROOT)
  for (const p of open) if (p !== TREE_ROOT) void loadTreeChildren(p)
}

/* ---- 顶栏路径条与目录历史 ---- */
/** 输入框里的路径：随成功加载的目录回写，输入非法路径失败后不会留在屏上 */
const pathDraft = ref(remotePath.value)
const historyOpen = ref(false)
const remoteHistory = ref<string[]>([])

function pushRemoteHistory(p: string) {
  const i = remoteHistory.value.indexOf(p)
  if (i >= 0) remoteHistory.value.splice(i, 1)
  remoteHistory.value.unshift(p)
  if (remoteHistory.value.length > 30) remoteHistory.value.length = 30
}

async function commitPath() {
  const p = pathDraft.value.trim().replace(/\/+$/, '') || '/'
  if (!p.startsWith('/')) {
    toast('请输入以 / 开头的绝对路径', 'warn', 2200)
    pathDraft.value = remotePath.value
    return
  }
  if (p === remotePath.value) { pathDraft.value = p; return }
  await loadRemote(p)
  pathDraft.value = remotePath.value // 失败时 loadRemote 保持原目录，输入框跟着回正
  // 只有真到位了才带着终端一起 cd：失败时不该把终端也带偏
  if (pathDraft.value === p) syncShellCwd(p)
}

function pickHistory(p: string) {
  historyOpen.value = false
  if (p !== remotePath.value) gotoCrumb('remote', p)
}

/** 历史下拉外点击 / Esc 收起：与传输记录浮层同一套 capture 监听，
 *  不用透明遮罩（遮罩会挡住文件区的拖拽上传命中） */
function onHistoryDismiss(e: MouseEvent | KeyboardEvent) {
  if (e.type === 'keydown') {
    if ((e as KeyboardEvent).key !== 'Escape') return
    historyOpen.value = false
    return
  }
  const el = e.target as HTMLElement | null
  if (!el) return
  if (el.closest('.tool-menu')) return
  historyOpen.value = false
}

watch(historyOpen, on => {
  if (on) {
    document.addEventListener('mousedown', onHistoryDismiss, true)
    document.addEventListener('keydown', onHistoryDismiss, true)
  } else {
    document.removeEventListener('mousedown', onHistoryDismiss, true)
    document.removeEventListener('keydown', onHistoryDismiss, true)
  }
})

/** 顶栏「上传」：本机栏可见且已选文件时走本机栏的选择集，否则用系统文件面板选文件 */
function onUploadOrPicker() {
  if (showLocalPane.value && localSel.value.length) { onUpload(); return }
  void uploadViaPicker()
}

/** 顶栏「上传」：本机栏默认隐藏，这里用系统文件面板选文件后直接落到当前远端目录 */
async function uploadViaPicker() {
  if (!connected.value) { toast('请先建立 SSH 连接', 'warn'); return }
  if (!isTauri) { toast('浏览器模式下无法选择本机文件', 'warn'); return }
  try {
    const picked = await open({ multiple: true, title: '选择要上传的文件', defaultPath: localHome.value || undefined })
    const list = typeof picked === 'string' ? [picked] : Array.isArray(picked) ? picked : []
    if (!list.length) return
    await uploadLocalPaths(list)
  } catch (e) {
    toast(String(e), 'warn', 2400)
  }
}

/* ---- 目录树宽度拖拽（持久化在 store.sftpTreeWidth） ---- */
const TREE_MIN_W = 132
const TREE_MAX_W = 420
const isTreeSplitting = ref(false)
let treeSplitting = false
let treeStartX = 0
let treeStartW = 0
let treePendingX = 0
let treeRaf = 0

function startTreeDrag(e: MouseEvent) {
  e.preventDefault()
  treeSplitting = true
  isTreeSplitting.value = true
  treeStartX = e.clientX
  treePendingX = e.clientX
  treeStartW = sftpTreeWidth.value
  document.body.style.userSelect = 'none'
  document.body.style.cursor = 'col-resize'
}

function applyTreeDrag() {
  treeRaf = 0
  sftpTreeWidth.value = Math.min(TREE_MAX_W, Math.max(TREE_MIN_W, treeStartW + treePendingX - treeStartX))
}

function endTreeDrag() {
  treeSplitting = false
  isTreeSplitting.value = false
  if (treeRaf) { cancelAnimationFrame(treeRaf); treeRaf = 0 }
  if (!dragging) {
    document.body.style.userSelect = ''
    document.body.style.cursor = ''
  }
}

/** 双击分割线恢复默认宽度 */
function onTreeDblClick(e: MouseEvent) {
  e.preventDefault()
  sftpTreeWidth.value = 200
}

/* ---- 按侧（local/remote）统一访问路径/原始列表/选择集/刷新 ---- */
function sidePath(side: Side) { return side === 'local' ? localPath.value : remotePath.value }
function sideRaw(side: Side) { return side === 'local' ? localRaw : remoteRaw }
function sideSel(side: Side) { return side === 'local' ? localSelected : remoteSelected }
function refreshSide(side: Side) {
  return side === 'local' ? loadLocal(localPath.value) : loadRemote(remotePath.value)
}
function entryExists(side: Side, name: string) {
  return sideRaw(side).value.some(e => e.name === name)
}

/**
 * 在目标侧为重名项分配一个不存在的候选名：
 *   a.txt → a (1).txt → a (2).txt；a.tar.gz → a.tar (1).gz
 *   无扩展名或纯点开头（.bashrc）→ name (1)
 * `used` 为本批粘贴已占用的名字（尚未落盘，列表里查不到）。
 */
function nextFreeName(side: Side, name: string, used: Set<string>) {
  const dot = name.lastIndexOf('.')
  // dot > 0：".bashrc" 的点在首位，按无扩展名处理
  const stem = dot > 0 ? name.slice(0, dot) : name
  const ext = dot > 0 ? name.slice(dot) : ''
  let n = 1
  for (;;) {
    const candidate = `${stem} (${n})${ext}`
    if (!used.has(candidate) && !entryExists(side, candidate)) return candidate
    n++
  }
}

/**
 * 统一派发按侧区分的后端命令：远程命令自动带 sessionId。
 * kind → 本地/远程命令名：
 *   mkdir       mkdir / sftp_mkdir
 *   create_file local_create_file / sftp_create_file
 *   rename      local_rename / sftp_rename
 *   remove      local_remove / sftp_remove
 *   copy        local_copy / sftp_copy
 */
async function invokeSide(
  side: Side,
  kind: 'mkdir' | 'create_file' | 'rename' | 'remove' | 'copy',
  args: Record<string, string>,
) {
  if (side === 'local') {
    const cmd = ({ mkdir: 'mkdir', create_file: 'local_create_file', rename: 'local_rename', remove: 'local_remove', copy: 'local_copy' } as const)[kind]
    await invoke(cmd, args)
  } else {
    const cmd = ({ mkdir: 'sftp_mkdir', create_file: 'sftp_create_file', rename: 'sftp_rename', remove: 'sftp_remove', copy: 'sftp_copy' } as const)[kind]
    const backendId = activeSession.value?.backendId
    if (!backendId) throw new Error('未连接')
    await invoke(cmd, { sessionId: backendId, ...args })
  }
}
function sortEntries(entries: FsEntry[], sort: SortState): FsEntry[] {
  return [...entries].sort((a, b) => {
    if (a.type !== b.type) return a.type === 'dir' ? -1 : 1
    let r = 0
    if (sort.key === 'name') r = a.name.localeCompare(b.name)
    if (sort.key === 'size') r = a.size - b.size
    if (sort.key === 'mtime') r = a.mtime.localeCompare(b.mtime)
    return r * sort.dir
  })
}
function fmtSize(bytes: number) {
  if (!bytes) return '—'
  if (bytes < 1024) return Math.round(bytes) + ' B'
  if (bytes < 1024 * 1024) return (bytes / 1024).toFixed(1) + ' KB'
  if (bytes < 1024 * 1024 * 1024) return (bytes / 1024 / 1024).toFixed(1) + ' MB'
  return (bytes / 1024 / 1024 / 1024).toFixed(2) + ' GB'
}
/* 两个字节数按同一单位成对格式化（已传输 / 总大小），单位取两者中较大者对应量级，
   避免浮点长小数与 "470 B / 1.0 KB" 这类单位错位：→ "0.5 KB / 1.0 KB" */
function fmtSizePair(a: number, b: number) {
  const max = Math.max(0, a, b)
  if (max < 1024) return `${Math.round(a)} B / ${Math.round(b)} B`
  if (max < 1024 * 1024) return `${(a / 1024).toFixed(1)} KB / ${(b / 1024).toFixed(1)} KB`
  if (max < 1024 * 1024 * 1024) return `${(a / 1048576).toFixed(1)} MB / ${(b / 1048576).toFixed(1)} MB`
  return `${(a / 1073741824).toFixed(2)} GB / ${(b / 1073741824).toFixed(2)} GB`
}
function crumbs(path: string) {
  const parts = path.split('/').filter(Boolean)
  const out: { label: string; path: string }[] = [{ label: '/', path: '/' }]
  let acc = ''
  parts.forEach(p => { acc += '/' + p; out.push({ label: p, path: acc }) })
  return out
}

/** 按全局设置过滤隐藏文件（. 开头）；开启时原样返回 */
function visibleEntries(list: FsEntry[]): FsEntry[] {
  return savedSettings.sftpShowHidden ? list : list.filter(e => !e.name.startsWith('.'))
}
const localEntries = computed(() => sortEntries(visibleEntries(localRaw.value), localSort.value))
const remoteEntries = computed(() => sortEntries(visibleEntries(remoteRaw.value), remoteSort.value))

/* ---- 传输队列：pending → running → success，同一时刻仅一个 running（串行）
   状态色规范：pending #606266 · running #409EFF · paused #E6A23C · error #F56C6C · success 项目绿 ---- */
type TransferStatus = 'pending' | 'running' | 'paused' | 'success' | 'error'
interface Transfer {
  id: number; name: string; dir: 'up' | 'down'
  size: number; pct: number; status: TransferStatus
  speed: number // 展示速率 B/s（真实传输由后端进度事件提供）
  /** 首次开始传输的墙钟时间戳（ms），用于完成事件 elapsed_ms；暂停时间计入 */
  startAt: number
  error?: string // 失败详情（查看错误弹窗展示）
  localPath?: string   // 本地绝对路径
  remotePath?: string  // 远端绝对路径
  /** 跨侧剪切粘贴：传输成功后要删除的源文件绝对路径 */
  cutSrc?: string
  /** ask 覆盖策略冲突弹窗的用户裁决（注入 policy.resolve 重投）：'overwrite' | 'skip' */
  resolve?: 'overwrite' | 'skip'
  /** 覆盖策略跳过（policy skip 或 ask 选跳过）：终态复用 success，备注显示「已跳过」 */
  skipped?: boolean
  /** 本次传输归属的会话（一条 SFTP 通道）：并发额度登记/释放按它配对，
   *  切换标签或断连后仍能对上号（见 lib/transferLedger） */
  ownerId?: string
  /** 编辑回写任务：无视 sftpOverwritePolicy 强制覆盖（副本就是要落回原文件，
   *  冲突已在回写前由「远端已变化」弹窗裁决过，不能再被跳过策略静默拦下） */
  forceOverwrite?: boolean
  /** 任务终结回调（完成/失败/跳过/取消），参数为远端是否已写入：
   *  编辑回写用它归还 busy 标记并刷新远端基线，其他调用方不传 */
  onSettled?: (ok: boolean) => void
}
const transfers = ref<Transfer[]>([])
let seq = 0

/** 后端 SFTP 进度事件载荷 */
interface TransferProgress {
  taskId: number
  transferred: number
  total: number
  speed: number
  done: boolean
}

/** 全局 SFTP 设置 → 每次调用的传输策略：Rust 侧刻意不读 app_config.json，
 *  行为策略全部由前端 invoke 时传参（见 src-tauri/src/ssh/sftp.rs TransferPolicy）；
 *  resolve 为 ask 冲突弹窗后的逐任务裁决（正常重投时携带） */
function transferPolicy(resolve?: 'overwrite' | 'skip') {
  return {
    overwritePolicy: savedSettings.sftpOverwritePolicy,
    uploadTemp: savedSettings.sftpUploadTemp,
    preserveMeta: savedSettings.sftpPreserveMeta,
    resumeCheck: savedSettings.sftpResumeCheck,
    verifyHash: savedSettings.sftpVerifyHash,
    resolve: resolve ?? null,
    // 两道带宽上限（KB/s，0=不限）：全局桶按 (主机:端口, 用户) 跨标签页共享，
    // 任务桶只约束本任务；实际节流在 src-tauri/src/ssh/sftp.rs Throttle
    globalRateKb: Math.max(0, Math.trunc(savedSettings.sftpGlobalRateKb) || 0),
    taskRateKb: Math.max(0, Math.trunc(savedSettings.sftpTaskRateKb) || 0),
  }
}

/** 解析 SFTP_CONFLICT 协议错误：`SFTP_CONFLICT: {目标路径}|{远端大小}|{本地大小}`
 *  （前缀协议见 src-tauri/src/ssh/mod.rs SftpConflict，同 HOSTKEY_UNKNOWN 惯例） */
function parseSftpConflict(
  msg: string,
): { path: string; remoteSize: number; localSize: number } | null {
  // 贪婪 path：容忍路径中含 '|' 的极端文件名
  const m = /^SFTP_CONFLICT:\s*(.+)\|(\d+)\|(\d+)$/.exec(msg)
  if (!m) return null
  return { path: m[1]!, remoteSize: Number(m[2]), localSize: Number(m[3]) }
}

/** 解析 SFTP_VERIFY_FAILED 协议错误：payload 为 `…{源摘要} ≠ …{目标摘要}`，
 *  摘要为 64 位小写 hex（前缀协议见 src-tauri/src/ssh/mod.rs SftpVerifyFailed） */
function parseSftpVerifyFailure(msg: string): { expect: string; actual: string } | null {
  if (!msg.startsWith('SFTP_VERIFY_FAILED:')) return null
  const ds = msg.match(/[0-9a-f]{64}/g)
  if (!ds || ds.length < 2) return { expect: '', actual: msg.slice('SFTP_VERIFY_FAILED:'.length).trim() }
  return { expect: ds[0]!, actual: ds[1]! }
}

/** 本次是否真的做了整文件 SHA256 校验：与后端 verify_required 同规则
 *  （显式开启 verifyHash，或 resumeCheck=sha256 隐含强制），且 0 字节后端跳过。
 *  完成事件的 verify 字段与队列备注据此如实标注，不再无条件报「校验通过」 */
function verifyRan(t: Transfer) {
  return t.size > 0 && (savedSettings.sftpVerifyHash || savedSettings.sftpResumeCheck === 'sha256')
}

/** 上传黑名单（设置项 sftpBlacklist，一行一条 glob）：命中的文件不创建上传任务。
 *  只支持 * 与 ? 两个通配符，按**文件名**（不含目录）匹配且大小写不敏感——macOS
 *  与 Windows 目录里常混有 .DS_Store / Thumbs.db 这类系统文件，故设置项缺省就带这两条 */
function isBlacklistedName(name: string): boolean {
  const patterns = String(savedSettings.sftpBlacklist ?? '')
    .split('\n')
    .map(p => p.trim())
    .filter(p => p !== '')
  if (!patterns.length) return false
  const lower = name.toLowerCase()
  return patterns.some(p => globToRe(p).test(lower))
}

function globToRe(glob: string): RegExp {
  const src = [...glob.toLowerCase()]
    .map(ch =>
      ch === '*' ? '[^/]*'
        : ch === '?' ? '[^/]'
          : '\\.^$+{}()|[]'.includes(ch) ? '\\' + ch : ch,
    )
    .join('')
  return new RegExp('^' + src + '$')
}

const runningCount = computed(() => transfers.value.filter(t => t.status === 'running').length)
const pendingCount = computed(() => transfers.value.filter(t => t.status === 'pending').length)
const pausedCount = computed(() => transfers.value.filter(t => t.status === 'paused').length)
const successCount = computed(() => transfers.value.filter(t => t.status === 'success').length)

/* 筛选：全部 / 上传 / 下载 / 已完成 */
const queueFilter = ref<'all' | 'up' | 'down' | 'done'>('all')
const filteredTransfers = computed(() => {
  const f = queueFilter.value
  return transfers.value.filter(t =>
    f === 'all' ? true
    : f === 'done' ? t.status === 'success'
    : t.dir === f,
  )
})

/* 传输记录浮层：由顶栏按钮点开，绝对定位覆盖在文件区右上，不再常驻底部占空间。
   tabs(28) + list(弹性)，最大高度受 .sftp 容器约束 */
const queueOpen = ref(false)

function toggleQueue() {
  queueOpen.value = !queueOpen.value
}

/** 浮层外点击 / Esc 关闭。用 capture 阶段的 document 监听：
 *  不遮蔽下层，拖拽上传等指针行为照常可用（透明遮罩会挡住文件区的 drop 事件） */
function onQueueDismiss(e: MouseEvent | KeyboardEvent) {
  if (e.type === 'keydown') {
    if ((e as KeyboardEvent).key !== 'Escape') return
    queueOpen.value = false
    return
  }
  const el = e.target as HTMLElement | null
  if (!el) return
  if (el.closest('.queue-pop') || el.closest('.queue-trigger')) return
  queueOpen.value = false
}

watch(queueOpen, on => {
  if (on) {
    document.addEventListener('mousedown', onQueueDismiss, true)
    document.addEventListener('keydown', onQueueDismiss, true)
  } else {
    document.removeEventListener('mousedown', onQueueDismiss, true)
    document.removeEventListener('keydown', onQueueDismiss, true)
  }
})

/** 顶栏按钮上的一行进度摘要：优先进行中任务（方向/状态/百分比/速率），
 *  否则按 等待 → 暂停 → 失败 → 完成 汇总；空闲留空。浮层收起时靠它保留进度可见性。
 *  这里不铺文件名：一长串 `ZCode-3.16.3-mac-arm64.dmg` 会把顶栏挤爆，
 *  而顶栏只需要「有没有在传、传到几成」，具体是哪个文件在队列浮层里看。 */
const queueSummary = computed(() => {
  const list = transfers.value
  if (!list.length) return ''
  const r = list.find(t => t.status === 'running')
  if (r) {
    const arrow = r.dir === 'up' ? '↑' : '↓'
    const speed = r.speed > 0 ? ' · ' + fmtSpeed(r.speed) : ''
    return `${arrow} 传输中 ${Math.floor(Math.min(100, r.pct))}%${speed}`
  }
  const pending = list.filter(t => t.status === 'pending').length
  if (pending) return `等待中 ${pending} 项`
  const paused = list.filter(t => t.status === 'paused').length
  if (paused) return `已暂停 ${paused} 项`
  const failed = list.filter(t => t.status === 'error').length
  if (failed) return `${failed} 项失败`
  const done = list.filter(t => t.status === 'success').length
  return done ? `已完成 ${done} 项` : ''
})

function fmtSpeed(bps: number) {
  if (!bps || bps <= 0) return '0 B/s'
  return fmtSize(bps) + '/s'
}
/* 状态文案 */
function statusText(t: Transfer) {
  return ({ pending: '等待中', running: '传输中', paused: '已暂停', success: '已完成', error: '失败' } as const)[t.status]
}
/* 队列内展示名 */
function taskName(t: Transfer) {
  return t.name
}
/* 已传输字节数 */
function doneBytes(t: Transfer) {
  return Math.min(t.size, Math.max(0, t.size * Math.min(100, Math.max(0, t.pct)) / 100))
}
/* 字节信息：已传 / 总大小 */
function bytesText(t: Transfer) {
  return fmtSizePair(doneBytes(t), t.size)
}
/* 速率：仅传输中 */
function rateText(t: Transfer) {
  return t.status === 'running' && t.speed > 0 ? fmtSpeed(t.speed) : ''
}
/* 备注：传输中显剩余时间，完成显校验通过，失败显错误，其余留空 */
function noteText(t: Transfer) {
  if (t.status === 'running' && t.speed > 0 && t.pct < 100) {
    const sec = (t.size - doneBytes(t)) / t.speed
    if (isFinite(sec) && sec >= 1) {
      if (sec < 60) return '剩 ' + Math.ceil(sec) + ' 秒'
      if (sec < 3600) return '剩 ' + Math.ceil(sec / 60) + ' 分'
      return '剩 ' + (sec / 3600).toFixed(1) + ' 时'
    }
  }
  if (t.status === 'success') {
    if (t.skipped) return '已跳过（目标已存在）'
    return verifyRan(t) ? '校验通过' : '传输完成'
  }
  if (t.status === 'error') return t.error || '传输失败'
  return ''
}
/* 右侧状态文案 */
function stateText(t: Transfer) {
  return statusText(t)
}

function startTask(
  dir: 'up' | 'down', name: string, size: number,
  extra: {
    localPath: string; remotePath: string; cutSrc?: string
    forceOverwrite?: boolean; onSettled?: (ok: boolean) => void
    /** 覆盖归属会话：编辑回写可能在用户切到别的主机后才触发，任务要回到文件所属那台 */
    ownerId?: string
  },
) {
  /* 不自动弹出传输记录浮层：入队后顶栏按钮的 LED / 数量 / 摘要已足够反馈，
     自动弹出会盖住文件区，批量传输与编辑器回写时尤其打扰 */
  transfers.value.push({
    id: ++seq, name, dir, size, pct: 0, status: 'pending', speed: 0, startAt: 0,
    localPath: extra.localPath,
    remotePath: extra.remotePath,
    cutSrc: extra.cutSrc,
    forceOverwrite: extra.forceOverwrite,
    onSettled: extra.onSettled,
    // 归属会话在入队时定死：切到别的标签页后，本任务仍回到自己那台主机执行
    ownerId: extra.ownerId ?? activeSession.value?.backendId,
  })
  reportLog('info', SFTP_TRANSFER_ENQUEUE, `${dir === 'up' ? '上传' : '下载'}加入队列`, {
    name, size, dir, from: extra.localPath, to: extra.remotePath,
  })
  pumpQueue()
}

/** 并发上限兜底：设置项被填成 0/负数/非数字时按 1 处理——
 *  填 0 的语义是「不要并发」，而不是「永远不启动队列」 */
function concurrencyCap(v: number): number {
  return Math.max(1, Math.trunc(v) || 0)
}

let pumpTimer: ReturnType<typeof setTimeout> | null = null
/** 槽位被别的标签页占着时定时回来再看一眼（对侧任务结束不会回调本面板） */
function schedulePump(ms = 1000) {
  if (pumpTimer) return
  pumpTimer = setTimeout(() => {
    pumpTimer = null
    pumpQueue()
  }, ms)
}

/** 队列闸门：在并发上限内尽量多地启动等待中的任务。
 *  两级上限（设置 → SFTP）：全局 = 跨所有标签页的在飞总数，
 *  单主机 = 同一会话（一条 SFTP 通道）的在飞数。
 *  任务按**入队时的会话**（ownerId）分组派发：传输队列是单个跨标签页常驻的面板共享的，
 *  切到别的主机后，仍在等待的任务要回到自己那台主机执行，而不是切过去的当前会话。
 *  每个任务的实际传输在 runTask 里，完成/失败/跳过后回到这里续链。 */
function pumpQueue() {
  const globalFree = () =>
    Math.max(0, concurrencyCap(savedSettings.sftpGlobalConcurrency) - runningTotal())
  let free = globalFree()
  let started = false
  // 入队时未连接（ownerId 缺失）的任务也要放行，让它立刻以「未建立 SSH 连接」失败
  const owners: (string | undefined)[] = []
  for (const t of transfers.value) {
    if (t.status === 'pending' && !owners.includes(t.ownerId)) owners.push(t.ownerId)
  }
  for (const owner of owners) {
    if (!owner) {
      for (const t of transfers.value) {
        if (t.status === 'pending' && !t.ownerId) void runTask(t, '')
      }
      continue
    }
    const hostFree = Math.max(
      0,
      concurrencyCap(savedSettings.sftpHostConcurrency) - runningOnBackend(owner),
    )
    // n 是本会话这一轮还能起的任务数：每起一个都要减，否则只受全局额度约束，
    // 「单主机最大并发」会被一次派发绕过（队列里有 N 个等待任务就全启动）
    let n = Math.min(free, hostFree)
    for (const t of transfers.value) {
      if (n <= 0) break
      if (t.status !== 'pending' || t.ownerId !== owner) continue
      void runTask(t, owner)
      free--
      n--
      started = true
    }
  }
  // 仍有等待任务 = 槽位被占（本面板之外的任务结束不会回调本面板），定时回来看一眼
  if (!started || free <= 0) {
    if (transfers.value.some(t => t.status === 'pending')) schedulePump()
  }
}

/** 单个任务的传输过程：状态流转 + 重试 + 冲突裁决。进度由后端 Channel 流式推送。 */
async function runTask(t: Transfer, backendId: string) {
  if (!backendId) {
    // 入队时就没有活动会话：直接判失败，不占用任何并发槽位
    t.status = 'error'
    t.error = '未建立 SSH 连接'
    reportLog('error', SFTP_TRANSFER_FAILED, `${t.dir === 'up' ? '上传' : '下载'}失败`, {
      name: t.name, err: '未建立 SSH 连接', transferred: 0, total: t.size,
    })
    toast(`传输失败：${taskName(t)}`, 'warn', 2200)
    return
  }
  t.status = 'running'
  t.ownerId = backendId
  markTransferRunning(backendId, t.id)
  if (t.startAt === 0) t.startAt = Date.now() // 仅首次开始计时；暂停后续传不重置
  const dirLabel = t.dir === 'up' ? '上传' : '下载'
  // pct > 0 为暂停/重试后的恢复，不再重复"开始"提示
  if (t.pct > 0) {
    reportLog('debug', SFTP_TRANSFER_RESUME, `${dirLabel}继续`, {
      name: t.name, pct: Math.floor(t.pct),
    })
  } else {
    reportLog('info', SFTP_TRANSFER_START, `${dirLabel}开始`, {
      name: t.name, size: t.size, resume_from: 0,
    })
    toast(`开始${dirLabel}：${taskName(t)}`, 'info', 1700)
  }

  const settle = (ok: boolean) => {
    clearTransferRunning(backendId, t.id)
    t.speed = 0
    void pumpQueue()
    // 回写调用方靠这个回调结束本轮跟踪（刷新基线 / 允许下一次保存再触发）
    t.onSettled?.(ok)
  }
  const failTask = (msg: string) => {
    t.status = 'error'
    t.error = msg
    reportLog('error', SFTP_TRANSFER_FAILED, `${dirLabel}失败`, {
      name: t.name, err: msg, transferred: Math.round(t.size * t.pct / 100), total: t.size,
    })
    toast(`${dirLabel}失败：${taskName(t)}`, 'warn', 2200)
    settle(false)
  }
  const finishTask = () => {
    t.pct = 100
    t.status = 'success'
    // 清零前捕获最后一帧速率（后端按滑动窗口计算的瞬时速率）
    const finalSpeedBps = t.speed
    reportLog('info', SFTP_TRANSFER_COMPLETE, `${dirLabel}完成`, {
      name: t.name, size: t.size,
      elapsed_ms: t.startAt ? Date.now() - t.startAt : 0,
      speed_bps: finalSpeedBps, verify: verifyRan(t) ? 'pass' : 'none',
    })
    toast(`${dirLabel}完成：${taskName(t)}`, 'ok', 2000)
    // 跨侧剪切粘贴：传输成功后删除源文件（失败仅记日志，目标已完整）
    if (t.cutSrc) {
      if (t.dir === 'up') {
        void invoke('local_remove', { path: t.cutSrc })
          .catch(e => reportLog('error', SFTP_MANAGE_REMOVE, '剪切源删除失败', { path: t.cutSrc, err: String(e) }))
      } else {
        void invoke('sftp_remove', { sessionId: backendId, path: t.cutSrc })
          .catch(e => reportLog('error', SFTP_MANAGE_REMOVE, '剪切源删除失败', { path: t.cutSrc, err: String(e) }))
      }
    }
    // 传输完成自动刷新对侧目录，免去手动点刷新：上传→远端，下载→本地
    if (t.dir === 'up') void loadRemote(remotePath.value)
    else void loadLocal(localPath.value)
    settle(true)
  }

  const skipTask = () => {
    // 覆盖策略跳过（policy skip / ask 弹窗选跳过）：未传字节但任务正常终结，
    // 复用 success 终态并以 skipped 标记区分备注文案，不判失败
    t.status = 'success'
    t.skipped = true
    t.pct = 100
    reportLog('info', SFTP_TRANSFER_COMPLETE, `${dirLabel}已跳过（目标已存在）`, {
      name: t.name, size: t.size,
      elapsed_ms: t.startAt ? Date.now() - t.startAt : 0,
      verify: 'skipped',
    })
    toast(`${dirLabel}跳过：${taskName(t)}`, 'info', 2000)
    settle(false)
  }

  if (!t.localPath || !t.remotePath) { failTask('传输路径缺失'); return }
  const cmd = t.dir === 'up' ? 'sftp_upload' : 'sftp_download'
  // 每个任务独立 Channel：后端流式推送进度，前端 onmessage 按 taskId 更新对应任务
  const channel = new Channel<TransferProgress>()
  channel.onmessage = (p) => {
    // 任务已终结（完成/跳过/失败/移除）后的收尾包竞态：不回写终态任务
    if (t.status !== 'running') return
    if (p.total > 0) t.size = p.total
    t.pct = p.total > 0 ? (p.transferred / p.total) * 100 : (p.done ? 100 : t.pct)
    t.speed = p.speed
  }
  const buildArgs = (resolve?: 'overwrite' | 'skip') => {
    // 编辑回写：强制覆盖 + 关掉断点续传。副本与远端常年只差几个字节、大小不变，
    // 而 resume 开启时后端有一条「目标已存在且与本地等大 → 视为已完成」的快路径，
    // 会把这次保存直接判成不用传（详见 src-tauri/src/ssh/sftp.rs upload_file 的快路径）
    const policy = t.forceOverwrite
      ? { ...transferPolicy(resolve), overwritePolicy: 'overwrite' as const }
      : transferPolicy(resolve)
    const resume = t.forceOverwrite ? false : savedSettings.sftpResume
    return t.dir === 'up'
      ? { sessionId: backendId, taskId: t.id, localPath: t.localPath, remotePath: t.remotePath, chunkKb: savedSettings.sftpChunkKb, resume, policy, channel }
      : { sessionId: backendId, taskId: t.id, remotePath: t.remotePath, localPath: t.localPath, chunkKb: savedSettings.sftpChunkKb, resume, policy, channel }
  }

  const maxRetry = Math.max(0, Math.trunc(savedSettings.sftpRetryCount) || 0)
  const retryWait = Math.max(0, Math.trunc(savedSettings.sftpRetryIntervalMs) || 0)
  // ask 冲突弹窗一轮封顶：用户裁决重投后若再回冲突（理论不可达）按失败处理，防弹窗死循环
  let conflictAsked = false
  let attempt = 0
  for (;;) {
    try {
      await invoke(cmd, buildArgs(t.resolve))
      finishTask()
      break
    } catch (e) {
      const msg = String(e)
      // 用户主动取消：静默移除，不弹失败 toast
      if (msg.includes('已取消') || msg.includes('Cancelled')) {
        t.status = 'error'
        t.error = '已取消'
        transfers.value = transfers.value.filter(x => x.id !== t.id)
        reportLog('warn', SFTP_TRANSFER_CANCEL, `${dirLabel}已取消`, {
          name: t.name, transferred: Math.round(t.size * t.pct / 100), total: t.size,
        })
        settle(false)
      } else if (msg.startsWith('SFTP_SKIPPED:')) {
        skipTask()
      } else if (parseSftpConflict(msg) && !conflictAsked) {
        conflictAsked = true
        const cf = parseSftpConflict(msg)!
        // 目标侧/来源侧大小随方向解释（payload 固定为 远端大小|本地大小）
        const exist = t.dir === 'up' ? cf.remoteSize : cf.localSize
        const src = t.dir === 'up' ? cf.localSize : cf.remoteSize
        const ok = await confirmDialog(
          `${t.dir === 'up' ? '远端' : '本地'}已存在同名文件：${t.name}（现有 ${fmtSize(exist)}，本次 ${fmtSize(src)}）。确定覆盖重传；跳过则保留现有文件不动。`,
          '文件已存在',
          { confirmText: '覆盖', cancelText: '跳过' },
        )
        // 裁决注入 policy.resolve 重投同一任务（后端按裁决覆盖或跳过）
        t.resolve = ok ? 'overwrite' : 'skip'
        continue
      } else if (parseSftpVerifyFailure(msg)) {
        // 字节传完但整文件 SHA256 两侧不一致：目标内容不可信，任务判失败并显著告警。
        // 后端已落 sftp.transfer.verify_failed（带两侧摘要），前端补按任务定位的失败事件
        const vf = parseSftpVerifyFailure(msg)!
        t.status = 'error'
        t.error = msg.slice('SFTP_VERIFY_FAILED:'.length).trim()
        reportLog('error', SFTP_TRANSFER_FAILED, `${dirLabel}完整性校验失败`, {
          name: t.name, reason: 'verify_failed', expect: vf.expect, actual: vf.actual,
          transferred: Math.round(t.size * t.pct / 100), total: t.size,
        })
        toast(`校验失败：${taskName(t)}（字节已传完但哈希不一致）`, 'err', 3200)
        settle(false)
      } else if (isTransientError(msg) && attempt < maxRetry) {
        // 网络/通道类瞬时错误：等待后自动重投。断点续传开着时后端从半截文件续接，
        // 故不清零 pct；重投前再确认任务未被用户取消/移除
        attempt++
        const note = `第 ${attempt}/${maxRetry} 次重试（${retryWait}ms 后）：${msg}`
        reportLog('warn', SFTP_TRANSFER_RETRY, `${dirLabel}失败，自动重试`, {
          name: t.name, attempt, max: maxRetry, wait_ms: retryWait, err: msg,
          transferred: Math.round(t.size * t.pct / 100), total: t.size,
        })
        t.error = note
        toast(`${dirLabel}中断，${note}`, 'warn', 2400)
        if (retryWait > 0) await new Promise(r => setTimeout(r, retryWait))
        if (t.status !== 'running' || !transfers.value.some(x => x.id === t.id)) {
          clearTransferRunning(backendId, t.id)
          void pumpQueue()
          // 任务被暂停/取消而中断：同样要归还回写跟踪的 busy，否则该文件永不再同步
          if (t.status !== 'running') t.onSettled?.(false)
          return
        }
        continue
      } else {
        failTask(msg)
      }
      break
    }
  }
}

/** 可自动重试的瞬时错误判定：用户意图类（取消/跳过/冲突待裁决）、业务拒绝类
 *  （体积超限）、完整性失败与本地参数错误都不该自动重投——重试只会重复同一个
 *  已被明确否决的结果，还可能覆盖用户刚改过的目标文件。
 *  未连接/路径缺失同理：等用户动作，不是等网络恢复。 */
function isTransientError(msg: string): boolean {
  if (msg.includes('已取消') || msg.includes('Cancelled')) return false
  if (msg.startsWith('SFTP_SKIPPED:') || msg.startsWith('SFTP_CONFLICT:')) return false
  if (msg.startsWith('SFTP_VERIFY_FAILED:')) return false
  if (msg.includes('超过 AI 传输上限')) return false
  if (msg === '未建立 SSH 连接' || msg === '传输路径缺失') return false
  return true
}

/** 取消/移除任务：进行中的任务先通知后端取消，再出队启动下一个 */
function removeTask(t: Transfer) {
  if (t.status === 'running') {
    // 控制指令跟着任务所属会话发，切了标签页也不会误伤另一台主机的同名 taskId
    if (t.ownerId) {
      void invoke('sftp_transfer_cancel', { sessionId: t.ownerId, taskId: t.id }).catch(() => {})
    }
  }
  transfers.value = transfers.value.filter(x => x.id !== t.id)
  reportLog('warn', SFTP_TRANSFER_CANCEL, `${t.dir === 'up' ? '上传' : '下载'}${t.status === 'success' ? '记录移除' : '已取消'}`, {
    name: t.name, transferred: Math.round(t.size * t.pct / 100), total: t.size,
  })
  if (t.status === 'running') void pumpQueue()
}

/** 暂停任务：通知后端暂停开关，invoke 仍挂起（保留串行槽位避免并发） */
function pauseTask(t: Transfer) {
  if (t.status === 'running') {
    if (t.ownerId) {
      void invoke('sftp_transfer_pause', { sessionId: t.ownerId, taskId: t.id, paused: true }).catch(() => {})
    }
    t.status = 'paused'
    t.speed = 0
    reportLog('warn', SFTP_TRANSFER_PAUSE, '已暂停', { name: t.name, pct: Math.floor(t.pct) })
  } else if (t.status === 'pending') {
    t.status = 'paused'
    reportLog('warn', SFTP_TRANSFER_PAUSE, '排队任务已挂起', { name: t.name, pct: 0 })
  }
}
/** 继续任务：通知后端恢复并直接回到 running（invoke 仍在） */
function resumeTask(t: Transfer) {
  if (t.status !== 'paused') return
  if (t.ownerId) {
    void invoke('sftp_transfer_pause', { sessionId: t.ownerId, taskId: t.id, paused: false }).catch(() => {})
  }
  t.status = 'running'
  // 与暂停（WARN）配对的显式用户动作，INFO 保证默认级别下可见
  reportLog('info', SFTP_TRANSFER_RESUME, `${t.dir === 'up' ? '上传' : '下载'}继续`, {
    name: t.name, pct: Math.floor(t.pct),
  })
}
/** 失败任务重试：进度归零、清错误后重新入队 */
function retryTask(t: Transfer) {
  if (t.status !== 'error') return
  t.pct = 0
  t.speed = 0
  t.error = undefined
  t.status = 'pending'
  reportLog('info', SFTP_TRANSFER_ENQUEUE, `${t.dir === 'up' ? '上传' : '下载'}重新加入队列`, {
    name: t.name, size: t.size, dir: t.dir, from: t.localPath, to: t.remotePath,
  })
  void pumpQueue()
}
/** 打开目录：下载完成 → 在系统文件管理器里定位到刚落地的文件；
 *  上传完成 → 远端目录本机打不开，改为关掉记录浮层、把远端面板导航过去 */
function openTaskDir(t: Transfer) {
  if (t.status !== 'success') return
  if (t.dir === 'down') {
    if (!isTauri) { toast('浏览器模式下无法打开本机目录', 'warn'); return }
    if (!t.localPath) { toast('该任务没有记录本机落点', 'warn'); return }
    revealItemInDir(t.localPath).catch(e => toast(`打开所在目录失败：${String(e)}`, 'warn', 2600))
    return
  }
  if (!t.remotePath) return
  const dir = parentPath(t.remotePath)
  queueOpen.value = false
  remotePath.value = dir
  remoteSelected.value = new Set()
  void loadRemote(dir)
  toast(`已跳转到远端 ${dir}`, 'info', 2000)
}
/** 头部批量操作：全部暂停 / 全部继续（独立按钮） */
function pauseAllTasks() {
  transfers.value.forEach(t => { if (t.status === 'running' || t.status === 'pending') pauseTask(t) })
}
function resumeAllTasks() {
  transfers.value.forEach(t => { if (t.status === 'paused') resumeTask(t) })
}

/** 清空已完成任务（进行中/排队中保留） */
function clearDoneTasks() {
  transfers.value = transfers.value.filter(t => t.status !== 'success')
}

/** 当前侧选中的条目 */
function selectedEntries(side: 'local' | 'remote'): FsEntry[] {
  const list = side === 'local' ? localRaw.value : remoteRaw.value
  const sel = side === 'local' ? localSelected.value : remoteSelected.value
  return list.filter(e => sel.has(e.name))
}

/** 各侧选中条目数（文件与目录），供 header 传输按钮状态/角标显示 */
const localSel = computed(() => selectedEntries('local'))
const remoteSel = computed(() => selectedEntries('remote'))

/** 将一个选中条目入队：只铺单个文件。目录在两个方向都由调用方改走整棵递归
 *  （上传 uploadLocalPaths / 下载 downloadRemotePaths），这里只兜住漏网的入口。
 *  返回值供调用方汇总提示：'ok' 已入队 / 'dir' 目录不走这条路 / 'blocked' 命中上传黑名单 */
function enqueueEntry(dir: 'up' | 'down', e: FsEntry): 'ok' | 'dir' | 'blocked' {
  if (e.type === 'dir') {
    toast('目录请改用「上传」/「下载」按钮整棵传输', 'warn')
    return 'dir'
  }
  if (dir === 'up' && isBlacklistedName(e.name)) {
    reportLog('info', SFTP_TRANSFER_BLOCKED, '上传命中黑名单，未创建任务', {
      name: e.name, size: e.size, dir, reason: 'blacklist',
      from: joinPath(localPath.value, e.name),
    })
    return 'blocked'
  }
  startTask(dir, e.name, e.size, {
    localPath: joinPath(localPath.value, e.name),
    remotePath: joinPath(remotePath.value, e.name),
  })
  return 'ok'
}

function onUpload() {
  if (!connected.value) { toast('请先建立 SSH 连接', 'warn'); return }
  const entries = selectedEntries('local')
  if (!entries.length) { toast('请先在左侧选择文件', 'warn'); return }
  // 目录走整棵递归上传（与拖拽同一条路径），单文件走原有直接入队
  const dirs = entries.filter(e => e.type === 'dir').map(e => joinPath(localPath.value, e.name))
  const blocked = entries.filter(e => e.type !== 'dir' && isBlacklistedName(e.name)).length
  entries.filter(e => e.type !== 'dir').forEach(e => enqueueEntry('up', e))
  // 静默丢弃会让用户以为文件在队列里，这里给出被拦下的数量（黑名单在设置 → SFTP）
  if (blocked) toast(`${blocked} 个文件命中上传黑名单，未创建任务`, 'info', 2200)
  if (dirs.length) void uploadLocalPaths(dirs)
  localSelected.value = new Set()
}
function onDownload() {
  if (!connected.value) { toast('请先建立 SSH 连接', 'warn'); return }
  const entries = selectedEntries('remote')
  if (!entries.length) { toast('请先在右侧选择文件', 'warn'); return }
  // 目录走整棵递归下载（与上传对称），单文件走原有直接入队
  const dirs = entries.filter(e => e.type === 'dir').map(e => joinPath(remotePath.value, e.name))
  entries.filter(e => e.type !== 'dir').forEach(e => enqueueEntry('down', e))
  if (dirs.length) void downloadRemotePaths(dirs)
  remoteSelected.value = new Set()
}

/** 把本机绝对路径显示成 ~ 短形式（顶栏空间有限，全路径放不下） */
function shortLocal(path: string): string {
  const home = localHome.value
  if (!home) return path
  if (path === home) return '~'
  return path.startsWith(home + '/') ? '~' + path.slice(home.length) : path
}

/** 更换下载保存目录：本机栏隐藏时，这是唯一能指定落点的地方。
 *  走系统目录面板而不是手输路径——写错一个字母，文件就静默落到别处。 */
async function pickDownloadDir() {
  if (!isTauri) { toast('浏览器模式下无法选择本机目录', 'warn'); return }
  try {
    const picked = await open({ directory: true, title: '选择下载保存目录', defaultPath: localPath.value })
    if (typeof picked !== 'string' || !picked) return
    await loadLocal(picked)
    if (localPath.value === picked) toast(`下载将保存到 ${shortLocal(picked)}`, 'info', 2200)
  } catch (e) {
    toast(String(e), 'warn', 2400)
  }
}

/* ---- 拖拽上传：从访达/资源管理器把文件或文件夹拖进窗口 → 上传到当前远端目录 ---- */

/** 后端 local_walk 返回结构（camelCase）：rel 恒用 / 连接，可直接拼到远端目录之后 */
interface LocalWalkResult {
  root: string
  name: string
  isDir: boolean
  files: Array<{ rel: string; abs: string; size: number }>
  /** 根之下各级目录的相对路径（含空目录，不含根本身）：远端据此复刻整棵骨架 */
  dirs: string[]
}

const dropHint = ref(false)
const sftpMainEl = ref<HTMLElement | null>(null)
let dropStop: (() => void) | null = null

/** 「拖入即上传」的目标区域只有文件栏主体：未连接时把 zone 收成 null，
 *  于是拖拽既不会亮提示也不会建任务；落在 AI 聊天附件区则各有各的 zone，互不误伤。 */
function dropZone(): HTMLElement | null {
  if (!connected.value || !activeSession.value?.backendId) return null
  return sftpMainEl.value
}

function baseNameOf(path: string): string {
  return path.split('/').pop() || path
}

/** 把一批本地绝对路径（文件/文件夹混合）铺成上传任务：目录整棵递归、
 *  远端在当前目录下复刻同名目录（含空子目录），文件直接落在当前目录。
 *  每个拖入项一次 sftp_mkdir_all 建好骨架——300 个文件的文件夹不会退化成
 *  300 次「逐级推导父目录 + 逐个 stat」；失败也只作废这一项，其余照常排队。
 *  拖拽上传与本地栏选中目录点「上传」共用这一条路径。 */
async function uploadLocalPaths(absPaths: string[]) {
  const backendId = activeSession.value?.backendId
  if (!backendId) { toast('请先建立 SSH 连接', 'warn'); return }
  const base = remotePath.value
  let enqueued = 0
  let blocked = 0
  let emptyDirs = 0
  const failed: string[] = []
  for (const p of absPaths) {
    let walk: LocalWalkResult
    try {
      // 目录整棵交给后端枚举：软链与非普通文件在那里统一跳过，前端不必再判一遍
      walk = await invoke<LocalWalkResult>('local_walk', { root: p })
    } catch (e) {
      failed.push(`${baseNameOf(p)}：${String(e)}`)
      continue
    }
    // 拖入目录 → 在远端当前目录下复刻一层同名目录；拖入文件 → 直接落在当前目录
    const dstRoot = walk.isDir ? joinPath(base, walk.name) : base
    const keep = walk.files.filter(f => {
      if (!isBlacklistedName(baseNameOf(f.rel))) return true
      blocked++
      reportLog('info', SFTP_TRANSFER_BLOCKED, '上传命中黑名单，未创建任务', {
        name: f.rel, size: f.size, dir: 'up', reason: 'blacklist', from: f.abs,
      })
      return false
    })
    if (walk.isDir) {
      try {
        await invoke('sftp_mkdir_all', {
          sessionId: backendId,
          paths: [dstRoot, ...walk.dirs.map(d => joinPath(dstRoot, d))],
        })
      } catch (e) {
        failed.push(`${walk.name}：${String(e)}`)
        continue
      }
      if (!keep.length) emptyDirs++
    }
    for (const f of keep) {
      startTask('up', walk.isDir ? `${walk.name}/${f.rel}` : f.rel, f.size, {
        localPath: f.abs,
        remotePath: joinPath(dstRoot, f.rel),
      })
      enqueued++
    }
  }
  const parts: string[] = []
  if (enqueued) parts.push(`${enqueued} 个文件`)
  if (emptyDirs) parts.push(`${emptyDirs} 个空目录`)
  if (parts.length) toast(`上传：${parts.join(' + ')} → ${base}`, 'ok', 2600)
  if (blocked) toast(`${blocked} 个文件命中上传黑名单，未创建任务`, 'info', 2200)
  if (failed.length) {
    toast(`${failed.length} 项未能加入队列：${failed[0]}`, 'warn', 3200)
    reportLog('warn', SFTP_TRANSFER_FAILED, '拖拽上传准备失败', {
      name: baseNameOf(absPaths[0] ?? ''), err: failed.join(' | '), transferred: 0, total: 0,
    })
  }
}

/** 后端 sftp_walk 返回结构（camelCase，与 LocalWalkResult 同构） */
interface RemoteWalkResult {
  root: string
  name: string
  isDir: boolean
  files: Array<{ rel: string; abs: string; size: number }>
  dirs: string[]
}

/** 把一批远端绝对路径（文件/文件夹混合）铺成下载任务：目录整棵递归、
 *  本地在当前目录下复刻同名目录（含空子目录），文件直接落在当前目录。
 *  与 uploadLocalPaths 对称：枚举交给后端，一轮 IPC 拿到整棵清单，
 *  本地先建好骨架再逐文件入队——失败只作废这一项，其余照常排队。 */
async function downloadRemotePaths(remotePaths: string[]) {
  const backendId = activeSession.value?.backendId
  if (!backendId) { toast('请先建立 SSH 连接', 'warn'); return }
  const base = localPath.value
  let enqueued = 0
  let emptyDirs = 0
  const failed: string[] = []
  for (const p of remotePaths) {
    let walk: RemoteWalkResult
    try {
      // 软链与设备/管道/套接字在后端统一跳过：跟随链接可能成环，特殊文件读不完
      walk = await invoke<RemoteWalkResult>('sftp_walk', { sessionId: backendId, root: p })
    } catch (e) {
      failed.push(`${baseNameOf(p)}：${String(e)}`)
      continue
    }
    const dstRoot = walk.isDir ? joinPath(base, walk.name) : base
    if (walk.isDir) {
      try {
        await invoke('local_mkdir_all', {
          paths: [dstRoot, ...walk.dirs.map(d => joinPath(dstRoot, d))],
        })
      } catch (e) {
        failed.push(`${walk.name}：${String(e)}`)
        continue
      }
      if (!walk.files.length) emptyDirs++
    }
    for (const f of walk.files) {
      startTask('down', walk.isDir ? `${walk.name}/${f.rel}` : f.rel, f.size, {
        localPath: joinPath(dstRoot, f.rel),
        remotePath: f.abs,
      })
      enqueued++
    }
  }
  const parts: string[] = []
  if (enqueued) parts.push(`${enqueued} 个文件`)
  if (emptyDirs) parts.push(`${emptyDirs} 个空目录`)
  if (parts.length) toast(`下载：${parts.join(' + ')} → ${shortLocal(base)}`, 'ok', 2600)
  if (failed.length) {
    toast(`${failed.length} 项未能加入队列：${failed[0]}`, 'warn', 3200)
    reportLog('warn', SFTP_TRANSFER_FAILED, '目录下载准备失败', {
      name: baseNameOf(remotePaths[0] ?? ''), err: failed.join(' | '), transferred: 0, total: 0,
    })
  }
}

/* ---- 外部编辑器协同：远端文件 → 本机临时副本 → 编辑器保存 → 自动回写 ---- */


/** local_files_meta 返回（camelCase，与传入 paths 一一对应，缺失项 exists=false） */
interface LocalFileMeta { exists: boolean; size: number; mtimeMs: number }
/** sftp_stat 返回 */
interface RemoteStat { exists: boolean; isDir: boolean; size: number; mtimeMs: number }

interface EditWatch {
  /** 副本所属会话（后端 SSH id）：会话关掉就停止跟踪，不再拿旧 id 去 stat/回写 */
  sessionId: string
  name: string
  remotePath: string
  tempPath: string
  /** 打开副本时的远端基线：回写前拿它比对，变了就说明别人在我们之后动过这个文件 */
  baseSize: number
  baseMtimeMs: number
  /** 上一轮轮询看到的本地状态：变了就当成一次「保存」 */
  lastSize: number
  lastMtimeMs: number
  /** 回写前置阶段（稳定性复检 / stat / 弹窗裁决）进行中 */
  busy: boolean
  /** 连续回写失败次数：到上限停止跟踪，避免断线期间每轮弹一次失败提示 */
  failCount: number
}

const editWatches = ref<EditWatch[]>([])
let editTimer: ReturnType<typeof setInterval> | null = null
/** 本地副本轮询间隔：编辑器保存后最多延迟这么久才开始回写 */
const EDIT_POLL_MS = 1200
/** 检测到变化后再复核一次：编辑器可能先写半截再改名，稳定了才传 */
const EDIT_SETTLE_MS = 400
const EDIT_MAX_FAIL = 3

/** 会话 id / 路径片段做目录名时，':' 这类字符不宜出现，统一替换 */
function safeSegment(s: string) {
  return s.replace(/[^A-Za-z0-9._-]/g, '_')
}

/** FNV-1a 32 位 → 8 位 hex：每个远端目录映射到一个固定的本月子目录。
 *  副本保留原文件名（编辑器按后缀选语法高亮），不同目录的同名文件靠这层分开 */
function fnv1a32(s: string): string {
  let h = 0x811c9dc5
  for (let i = 0; i < s.length; i++) {
    h ^= s.charCodeAt(i)
    h = Math.imul(h, 0x01000193) >>> 0
  }
  return h.toString(16).padStart(8, '0')
}

/** 该副本是否还有未终结的传输任务：防止同一文件重复入队回写（暂停/重试期间也算） */
function hasLiveTaskFor(path: string): boolean {
  return transfers.value.some(t =>
    t.localPath === path
    && (t.status === 'pending' || t.status === 'running' || t.status === 'paused'))
}

function startEditPoll() {
  if (editTimer || !editWatches.value.length) return
  editTimer = setInterval(() => { void pollEditWatches() }, EDIT_POLL_MS)
}

/** 没有副本可看了就把定时器归还：空闲时不为编辑跟踪付出任何轮询 */
function stopEditPollIfIdle() {
  if (!editWatches.value.length && editTimer) {
    clearInterval(editTimer)
    editTimer = null
  }
}

/** 一批远端文件用本机编辑器打开（右键「用编辑器打开」，支持多选批量）。
 *  每个文件取回一份临时副本并记下远端基线，之后由轮询发现编辑器保存、自动回写。
 *  已在跟踪中的文件不重新取回——那会覆盖编辑器里还没保存的改动，只是再叫编辑器打开它。 */
async function openInEditor(entries: FsEntry[]) {
  const backendId = activeSession.value?.backendId
  if (!backendId) { toast('请先建立 SSH 连接', 'warn'); return }
  if (!isTauri) { toast('浏览器模式下无法调用本机编辑器', 'warn'); return }
  const files = entries.filter(e => e.type !== 'dir')
  if (!files.length) { toast('只能编辑文件，目录请先双击进入', 'warn'); return }

  let root = ''
  try {
    const tmp = await tempDir()
    root = `${tmp.replace(/[\\/]+$/, '')}/rhost-edit/${safeSegment(backendId)}`
  } catch (e) {
    toast(`无法定位临时目录：${e}`, 'warn', 2600)
    return
  }

  const opened: string[] = []
  const fresh: EditWatch[] = []
  const failed: string[] = []
  for (const f of files) {
    const remote = joinPath(remotePath.value, f.name)
    const existing = editWatches.value.find(w => w.sessionId === backendId && w.remotePath === remote)
    if (existing) { opened.push(existing.tempPath); continue }
    // 同一远端目录共用一个子目录：编辑器里「打开文件夹」时能看到兄弟文件
    const local = `${root}/${fnv1a32(parentPath(remote))}/${f.name}`
    try {
      const size = await invoke<number>('sftp_fetch', {
        sessionId: backendId, remotePath: remote, localPath: local,
      })
      const st = await invoke<RemoteStat>('sftp_stat', { sessionId: backendId, path: remote })
      fresh.push({
        sessionId: backendId, name: f.name, remotePath: remote, tempPath: local,
        baseSize: st.size, baseMtimeMs: st.mtimeMs,
        // 本地基线先占位成副本自身，落盘真实状态在取回完成后一次性补齐
        lastSize: size, lastMtimeMs: 0, busy: false, failCount: 0,
      })
      opened.push(local)
    } catch (e) {
      failed.push(`${f.name}：${String(e)}`)
      reportLog('error', SFTP_EDIT_OPEN, '编辑器打开失败（取回副本）', {
        name: f.name, remote, err: String(e),
      })
    }
  }
  if (!opened.length) {
    toast(`打开编辑器失败：${failed[0] ?? '无可编辑文件'}`, 'warn', 3200)
    return
  }

  // 记下副本刚落盘时的状态作为「还没保存」基线，否则第一轮轮询就把刚取回的文件当成改动回写
  try {
    const metas = await invoke<LocalFileMeta[]>('local_files_meta', {
      paths: fresh.map(w => w.tempPath),
    })
    fresh.forEach((w, i) => {
      const m = metas[i]
      if (!m || !m.exists) return
      w.lastMtimeMs = m.mtimeMs
      w.lastSize = m.size
    })
  } catch (e) {
    toast(`读取副本状态失败：${e}`, 'warn', 2600)
    return
  }
  editWatches.value = [...editWatches.value, ...fresh]

  let editor = ''
  try {
    editor = await invoke<string>('open_in_editor', {
      paths: opened,
      command: String(savedSettings.sftpEditorCommand ?? '').trim() || null,
    })
  } catch (e) {
    // 编辑器没起来：这些副本没人会保存，撤掉跟踪，免得留下永远不回写的轮询
    // （按 tempPath 比对：ref 数组取出的元素是响应式代理，与 fresh 里的原始对象不是同一个引用）
    const mine = new Set(fresh.map(w => w.tempPath))
    editWatches.value = editWatches.value.filter(w => !mine.has(w.tempPath))
    stopEditPollIfIdle()
    toast(`拉起编辑器失败：${e}`, 'err', 3200)
    return
  }
  for (const w of fresh) {
    reportLog('info', SFTP_EDIT_OPEN, '编辑器打开远端文件', {
      name: w.name, remote: w.remotePath, temp: w.tempPath, size: w.lastSize, editor,
    })
  }
  startEditPoll()
  const suffix = failed.length ? `，${failed.length} 个未能打开` : ''
  toast(`已在 ${editor} 打开 ${opened.length} 个文件${suffix}：保存后自动回写远端`, 'ok', 2800)
  if (failed.length) toast(`打开失败：${failed[0]}`, 'warn', 3200)
}

/** 一轮轮询：只问本地状态（一次 IPC 覆盖全部副本），变了的那份进入回写裁决。 */
async function pollEditWatches() {
  if (!editWatches.value.length) { stopEditPollIfIdle(); return }
  // 会话已关闭 / 重连换过后端 id：副本与远端不再对应，停止跟踪（临时文件保留，用户仍可手动上传）
  const known = new Set(sessions.value.map(s => s.backendId).filter((x): x is string => !!x))
  const online = new Set(
    sessions.value.filter(s => s.state === 'online' && s.backendId).map(s => s.backendId!),
  )
  const dropped = editWatches.value.filter(w => !known.has(w.sessionId))
  if (dropped.length) {
    editWatches.value = editWatches.value.filter(w => known.has(w.sessionId))
    stopEditPollIfIdle()
    toast(`${dropped.length} 个文件的编辑回写已停止（会话不可用）`, 'info', 2400)
  }
  // 连接不可用（重连中/已离线）时先不动：等它回来，而不是把失败计到副本头上
  const idle = editWatches.value.filter(w =>
    online.has(w.sessionId) && !w.busy && !hasLiveTaskFor(w.tempPath))
  if (!idle.length) return
  let metas: LocalFileMeta[]
  try {
    metas = await invoke<LocalFileMeta[]>('local_files_meta', { paths: idle.map(w => w.tempPath) })
  } catch { return }
  for (let i = 0; i < idle.length; i++) {
    const w = idle[i]!
    const m = metas[i]
    if (!m || !m.exists) continue
    if (m.mtimeMs === w.lastMtimeMs && m.size === w.lastSize) continue
    w.lastMtimeMs = m.mtimeMs
    w.lastSize = m.size
    void syncEditWatch(w)
  }
}

/** 单个副本的回写：复核稳定性 → 比对远端基线 →（必要时）弹窗裁决 → 入队上传 */
async function syncEditWatch(w: EditWatch) {
  w.busy = true
  let handedOff = false
  try {
    await new Promise(r => setTimeout(r, EDIT_SETTLE_MS))
    const check = await invoke<LocalFileMeta[]>('local_files_meta', { paths: [w.tempPath] })
    const now = check[0]
    if (!now || !now.exists) return
    // 还在被写（mtime/size 又动了）：这轮放弃，但**不更新基线**——下一轮仍把它当成一次新改动重新复核，
    // 否则会漏掉这次保存（基线追上了写入中的状态，之后再没有「变化」可言）
    if (now.mtimeMs !== w.lastMtimeMs || now.size !== w.lastSize) return
    const remote = await invoke<RemoteStat>('sftp_stat', { sessionId: w.sessionId, path: w.remotePath })
    if (!remote.exists || remote.mtimeMs !== w.baseMtimeMs || remote.size !== w.baseSize) {
      const gone = !remote.exists
      const ok = await confirmDialog(
        gone
          ? `远端文件已被删除：${w.name}。回写会按本机副本重新创建它；放弃回写则保持远端不存在（副本在 ${w.tempPath}）。`
          : `远端文件在你编辑期间已被改动：${w.name}（远端 ${fmtSize(remote.size)}，打开时 ${fmtSize(w.baseSize)}）。回写会用本机副本覆盖远端版本，对方的改动可能丢失；放弃回写则保留远端现状（副本在 ${w.tempPath}）。`,
        gone ? '远端文件已删除' : '远端文件已变化',
        { confirmText: '仍然覆盖', cancelText: '放弃回写' },
      )
      reportLog('warn', SFTP_EDIT_CONFLICT, '远端已变化，等待用户裁决', {
        name: w.name, remote: w.remotePath,
        base_mtime_ms: w.baseMtimeMs, now_mtime_ms: remote.mtimeMs,
        base_size: w.baseSize, now_size: remote.size,
        decision: ok ? 'overwrite' : 'keep-remote',
      })
      if (!ok) {
        // 以远端现状重新取基线：同一次改动不再重复弹窗，下一次保存才再问
        w.baseMtimeMs = remote.mtimeMs
        w.baseSize = remote.size
        toast(`已放弃回写：${w.name}`, 'info', 2400)
        return
      }
    }
    handedOff = writeBackEdit(w, now.size)
  } catch (e) {
    w.failCount++
    const msg = String(e)
    reportLog('error', SFTP_EDIT_SYNC, `编辑回写失败（第 ${w.failCount} 次）`, {
      name: w.name, remote: w.remotePath, err: msg,
    })
    if (w.failCount >= EDIT_MAX_FAIL) {
      stopTrackingEdit(w, `连续 ${w.failCount} 次失败`)
      toast(`停止回写：${w.name}（${msg}）`, 'warn', 3200)
    } else {
      toast(`回写失败：${w.name}（${msg}）`, 'warn', 2800)
    }
  } finally {
    if (!handedOff) w.busy = false
  }
}

/** 回写走正常上传队列：强制覆盖（副本就是要落回原文件）、任务名带「回写」前缀便于辨认。
 *  返回值表示任务已入队（busy 交给 onSettled 归还）。 */
function writeBackEdit(w: EditWatch, size: number): boolean {
  const startAt = Date.now()
  startTask('up', `回写 ${w.name}`, size, {
    localPath: w.tempPath,
    remotePath: w.remotePath,
    ownerId: w.sessionId,
    forceOverwrite: true,
    onSettled: (ok) => {
      w.busy = false
      if (!ok) return
      w.failCount = 0
      reportLog('info', SFTP_EDIT_SYNC, '编辑保存已回写', {
        name: w.name, remote: w.remotePath, size, elapsed_ms: Date.now() - startAt,
      })
      toast(`已回写：${w.name}`, 'ok', 1600)
      // 我们自己这次写入同样会改动远端 mtime/size：重新取基线，否则下一次保存会把它当成别人的改动
      void rebaseRemote(w)
    },
  })
  return true
}

async function rebaseRemote(w: EditWatch) {
  try {
    const st = await invoke<RemoteStat>('sftp_stat', { sessionId: w.sessionId, path: w.remotePath })
    w.baseMtimeMs = st.mtimeMs
    w.baseSize = st.size
  } catch {
    // 取不到就保留旧基线：代价是下次保存多问一次，不会变成静默覆盖
  }
}

/** 撤掉一个副本的跟踪（失败到上限时调用）：编辑器里未保存的内容仍在临时文件里 */
function stopTrackingEdit(w: EditWatch, reason: string) {
  editWatches.value = editWatches.value.filter(x => x.tempPath !== w.tempPath)
  reportLog('warn', SFTP_EDIT_SYNC, `编辑回写已停止（${reason}）`, {
    name: w.name, remote: w.remotePath, temp: w.tempPath,
  })
  stopEditPollIfIdle()
}

/* ---- 文件管理操作（右键菜单与快捷键共用同一套业务函数） ---- */

/** 删除目标：不进回收站、不做二次确认，逐项直接删除 → 刷新；软链只删链接本身（后端保证） */
async function deleteTargets(side: Side, targets: FsEntry[]) {
  if (!targets.length) return
  try {
    for (const t of targets) {
      await invokeSide(side, 'remove', { path: joinPath(sidePath(side), t.name) })
    }
    sideSel(side).value = new Set()
    reportLog('info', SFTP_MANAGE_REMOVE, `删除 ${targets.length} 项`, {
      side: side === 'local' ? '本地' : '远程', count: targets.length,
    })
    toast(`已删除 ${targets.length} 项`, 'ok', 1800)
    void refreshSide(side)
  } catch (e) {
    toast(`删除失败：${(e as Error).message}`, 'warn')
    void refreshSide(side)
  }
}

/* ---- 修改权限（远端 chmod，FinalShell 形态对话框） ---- */
const permVisible = ref(false)
const permBusy = ref(false)
const permEntry = ref<FsEntry | null>(null)
/** 打开瞬间锁定绝对路径：弹窗期间远端 CWD 可能被终端同步改动 */
const permPath = ref('')
const permModalEntry = computed(() => {
  const e = permEntry.value
  return e ? { name: e.name, perm: e.perm, isDir: e.type === 'dir', isSymlink: e.isSymlink } : null
})
function fmtMode(mode: number) {
  return mode.toString(8).padStart(3, '0')
}
function openPermDialog(entry: FsEntry) {
  permEntry.value = entry
  permPath.value = joinPath(sidePath('remote'), entry.name)
  permVisible.value = true
}
/** 应用权限：失败也要刷新——递归改权限可能已落了一半，列表不能继续显示旧值 */
async function applyPerm(p: { mode: number; recursive: boolean; scope: 'all' | 'files' | 'dirs' }) {
  const backendId = activeSession.value?.backendId
  if (!backendId) {
    toast('未连接', 'warn')
    return
  }
  permBusy.value = true
  try {
    await invoke('sftp_set_perm', {
      sessionId: backendId,
      path: permPath.value,
      mode: p.mode,
      recursive: p.recursive,
      scope: p.scope,
    })
    reportLog('info', SFTP_MANAGE_CHMOD, `修改权限为 ${fmtMode(p.mode)}`, {
      path: permPath.value, mode: fmtMode(p.mode), recursive: p.recursive, scope: p.scope,
    })
    toast(`已修改 ${permEntry.value?.name ?? ''} 权限`, 'ok', 1800)
    permVisible.value = false
  } catch (e) {
    toast(`修改权限失败：${(e as Error).message}`, 'warn')
  } finally {
    permBusy.value = false
    void refreshSide('remote')
  }
}

/** 复制/剪切当前选择：记录复制瞬间源目录绝对路径与元数据，之后导航仍可粘贴 */
function copySelection(side: Side, mode: 'copy' | 'cut') {
  const entries = selectedEntries(side)
  if (!entries.length) return
  clipboard.value = {
    srcSide: side,
    base: sidePath(side),
    mode,
    items: entries.map(e => ({ name: e.name, isDir: e.type === 'dir', size: e.size })),
  }
  toast(mode === 'copy' ? `已复制 ${entries.length} 项` : `已剪切 ${entries.length} 项`, 'info')
}

/**
 * 粘贴：同侧 copy → 递归复制、cut → 重命名；跨侧 → 入传输队列
 * （跨侧 cut 传输成功后自动删除源文件）。目标重名不覆盖、不询问，
 * 自动分配 "原名 (n)" 的新名字。
 */
async function pasteTargets(side: Side) {
  const cb = clipboard.value
  if (!cb) return
  const dstDir = sidePath(side)
  interface Job { src: string; dst: string; name: string; isDir: boolean; size: number }
  const jobs: Job[] = []
  for (const it of cb.items) {
    const src = joinPath(cb.base, it.name)
    const dst = joinPath(dstDir, it.name)
    if (src === dst) continue
    jobs.push({ src, dst, name: it.name, isDir: it.isDir, size: it.size })
  }
  if (!jobs.length) { toast('源与目标位置相同', 'info'); return }

  /* 重名冲突：自动重命名（a.txt → a (1).txt），不覆盖任何已有项。
     used 记录本批已占用的新名字，防止多个 job 拿到同一候选 */
  const used = new Set<string>()
  for (const j of jobs) {
    if (used.has(j.name) || entryExists(side, j.name)) {
      const free = nextFreeName(side, j.name, used)
      j.name = free
      j.dst = joinPath(dstDir, free)
    }
    used.add(j.name)
  }

  if (cb.srcSide === side) {
    /* 同侧：纯远程/纯本地操作 */
    try {
      for (const j of jobs) {
        if (cb.mode === 'cut') await invokeSide(side, 'rename', { oldPath: j.src, newPath: j.dst })
        else await invokeSide(side, 'copy', { src: j.src, dst: j.dst })
      }
      toast(cb.mode === 'cut' ? '已移动' : '已粘贴', 'ok')
      if (cb.mode === 'cut') clipboard.value = null
      void refreshSide(side)
    } catch (e) {
      toast(`粘贴失败：${(e as Error).message}`, 'warn')
      void refreshSide(side)
    }
  } else {
    /* 跨侧：本质下载/上传，入现有串行传输队列 */
    const dir: 'up' | 'down' = side === 'remote' ? 'up' : 'down'
    for (const j of jobs) {
      if (j.isDir) { toast('暂不支持目录传输，请选择文件', 'warn'); continue }
      if (dir === 'up' && isBlacklistedName(j.name)) {
        reportLog('info', SFTP_TRANSFER_BLOCKED, '上传命中黑名单，未创建任务', {
          name: j.name, size: j.size, dir, reason: 'blacklist', from: j.src,
        })
        continue
      }
      const localAbs = dir === 'up' ? j.src : j.dst
      const remoteAbs = dir === 'up' ? j.dst : j.src
      startTask(dir, j.name, j.size, {
        localPath: localAbs,
        remotePath: remoteAbs,
        cutSrc: cb.mode === 'cut' ? j.src : undefined,
      })
    }
    if (cb.mode === 'cut') clipboard.value = null
  }
}

/** 复制选中项完整路径到系统剪贴板（多项换行分隔） */
async function copyPaths(side: Side, targets: FsEntry[]) {
  if (!targets.length) return
  const text = targets.map(t => joinPath(sidePath(side), t.name)).join('\n')
  await writeClipboardText(text)
  toast('路径已复制', 'ok')
}

/** 写系统剪贴板：优先 Clipboard API，失败降级 textarea + execCommand */
async function writeClipboardText(text: string) {
  try {
    await navigator.clipboard.writeText(text)
  } catch {
    const ta = document.createElement('textarea')
    ta.value = text
    ta.style.position = 'fixed'
    ta.style.opacity = '0'
    document.body.appendChild(ta)
    ta.select()
    document.execCommand('copy')
    document.body.removeChild(ta)
  }
}

/** 在终端打开：远程 → 应用内终端 cd 该目录；本地 → 系统终端打开该目录。
 *  传文件时定位到其所在目录。 */
function openInTerminal(side: Side, target?: FsEntry) {
  const p = target && target.type === 'dir'
    ? joinPath(sidePath(side), target.name)
    : sidePath(side)
  if (side === 'remote') {
    syncShellCwd(p)
    toast('已在终端打开', 'info', 1500)
  } else {
    invoke('local_open_terminal', { path: p })
      .then(() => toast('已在终端打开', 'info', 1500))
      .catch(e => toast(`打开终端失败：${e}`, 'warn'))
  }
}

/** 全选当前侧可见条目 */
function selectAll(side: Side) {
  sideSel(side).value = new Set(visibleEntries(sideRaw(side).value).map(e => e.name))
}

/* ---- 右键事件：源头一次性写全坐标/侧/目标 ---- */
function onRowContext(e: MouseEvent, side: Side, entry: FsEntry) {
  // 阻止 WKWebView 原生菜单，否则与自绘菜单同时弹出
  e.preventDefault()
  lastSide.value = side
  const sel = sideSel(side)
  // 右键未选中项 → 仅选该项；右键已选中项 → 保持多选
  if (!sel.value.has(entry.name)) sel.value = new Set([entry.name])
  menu.value = { x: e.clientX, y: e.clientY, side, targets: selectedEntries(side) }
}
function onBlankContext(e: MouseEvent, side: Side) {
  e.preventDefault()
  lastSide.value = side
  sideSel(side).value = new Set()
  menu.value = { x: e.clientX, y: e.clientY, side, targets: [] }
}

/* v-model 代理：ContextMenu 关闭即清空菜单状态 */
const menuVisible = computed({
  get: () => menu.value !== null,
  set: v => { if (!v) menu.value = null },
})
const menuX = computed(() => menu.value?.x ?? 0)
const menuY = computed(() => menu.value?.y ?? 0)
const menuItems = computed<MenuItem[]>(() => {
  const m = menu.value
  return m ? buildMenu(m.side, m.targets) : []
})

/* 图标 path d（24x24） */
const ICON = {
  mkdir: 'M4 7a2 2 0 012-2h3l2 2h7a2 2 0 012 2v8a2 2 0 01-2 2H6a2 2 0 01-2-2V7z M12 11v5M9.5 13.5h5',
  mkfile: 'M14 3H6a2 2 0 00-2 2v14a2 2 0 002 2h12a2 2 0 002-2V9z M14 3v6h6 M12 13v4M10 15h4',
  paste: 'M9 4h6a2 2 0 012 2h2a1 1 0 011 1v13a1 1 0 01-1 1H6a1 1 0 01-1-1V7a1 1 0 011-1h2a2 2 0 012-2z M9 4a2 2 0 012-2h2a2 2 0 012 2 M12 11v5M9.5 13.5h5',
  copy: 'M9 9h10v10H9z M5 15H4V4h11v1',
  cut: 'M8.5 7.5 19 18 M8.5 16.5 19 6 M6 6a2 2 0 100 4 2 2 0 000-4z M6 14a2 2 0 100 4 2 2 0 000-4z',
  rename: 'M4 20h4L19 9l-4-4L4 16v4z M14 6l4 4',
  perm: 'M7 11V8a5 5 0 0110 0v3 M5 11h14v10H5z M12 15v2',
  edit: 'M14 3H6a2 2 0 00-2 2v14a2 2 0 002 2h12a2 2 0 002-2V9z M14 3v6h6 M10 12l-2 2 2 2 M14 12l2 2-2 2',
  del: 'M4 7h16M9 7V4h6v3M6 7l1 13h10l1-13M10 11v5M14 11v5',
  path: 'M7 8h11M7 12h11M7 16h7 M4 7h.01M4 11h.01M4 15h.01',
  term: 'M4 5h16v14H4z M8 9l3 3-3 3 M13 15h3',
  all: 'M4 4h16v16H4z M9 9h6v6H9z',
  enter: 'M9 6l6 6-6 6 M5 12h10',
  file: 'M14 3H6a2 2 0 00-2 2v14a2 2 0 002 2h12a2 2 0 002-2V9z M14 3v6h6',
  up: 'M12 19V5M5 12l7-7 7 7',
  down: 'M12 5v14M5 12l7 7 7-7',
  refresh: 'M21 12a9 9 0 11-3-6.7L21 8 M21 3v5h-5',
}

/** 按场景构造菜单模型（空白/单项/多选） */
function buildMenu(side: Side, targets: FsEntry[]): MenuItem[] {
  if (!targets.length) {
    return [
      { label: '新建文件夹', icon: ICON.mkdir, action: () => startEdit(side, 'mkdir') },
      { label: '新建文件', icon: ICON.mkfile, action: () => startEdit(side, 'mkfile') },
      { divider: true },
      { label: '粘贴', icon: ICON.paste, shortcut: '⌘V', disabled: !clipboard.value, action: () => void pasteTargets(side) },
      { divider: true },
      { label: '全选', icon: ICON.all, shortcut: '⌘A', action: () => selectAll(side) },
      { label: '刷新', icon: ICON.refresh, action: () => void refreshSide(side) },
    ]
  }

  const single = targets.length === 1 ? targets[0]! : null
  const onlyFiles = targets.every(t => t.type !== 'dir')
  const items: MenuItem[] = []

  if (single) {
    items.push({
      label: single.type === 'dir' ? '进入目录' : '打开',
      icon: single.type === 'dir' ? ICON.enter : ICON.file,
      action: () => onRowDblClick(side, single),
    })
    items.push({ divider: true })
  }

  if (onlyFiles) {
    items.push({
      label: side === 'local' ? '上传到远程' : '下载到本地',
      icon: side === 'local' ? ICON.up : ICON.down,
      disabled: !connected.value,
      action: () => {
        sideSel(side).value = new Set(targets.map(t => t.name))
        if (side === 'local') onUpload()
        else onDownload()
      },
    })
    items.push({ divider: true })
  }

  if (side === 'remote' && onlyFiles) {
    items.push({
      label: targets.length > 1 ? `用编辑器打开 ${targets.length} 个文件` : '用编辑器打开',
      icon: ICON.edit, shortcut: '⌘↵',
      disabled: !connected.value,
      action: () => void openInEditor(targets),
    })
    items.push({ divider: true })
  }

  items.push({ label: '复制', icon: ICON.copy, shortcut: '⌘C', action: () => copySelection(side, 'copy') })
  items.push({ label: '剪切', icon: ICON.cut, shortcut: '⌘X', action: () => copySelection(side, 'cut') })
  if (single) {
    items.push({ label: '重命名', icon: ICON.rename, shortcut: 'F2', action: () => startEdit(side, 'rename', single) })
    /* 权限对话框只挂远端：本机侧未接 chmod 命令 */
    if (side === 'remote') {
      items.push({
        label: '修改权限', icon: ICON.perm,
        disabled: !connected.value,
        action: () => openPermDialog(single),
      })
    }
  }
  items.push({
    label: targets.length > 1 ? `删除 ${targets.length} 项` : '删除',
    icon: ICON.del, shortcut: 'Del', danger: true,
    action: () => void deleteTargets(side, targets),
  })
  items.push({ divider: true })
  items.push({ label: '复制路径', icon: ICON.path, action: () => void copyPaths(side, targets) })
  /* 本地/远程菜单一致：单项时可在终端打开（文件 → 所在目录） */
  if (single) {
    items.push({
      label: single.type === 'dir' ? '在终端打开' : '在终端打开所在目录',
      icon: ICON.term,
      action: () => openInTerminal(side, single),
    })
  }
  return items
}

/* 行交互：单击选择（Ctrl/⌘ 多选）、双击目录进入、双击文件传输 */
function onRowClick(side: Side, entry: FsEntry, e: MouseEvent) {
  lastSide.value = side
  const sel = side === 'local' ? localSelected : remoteSelected
  if (e.ctrlKey || e.metaKey) {
    const next = new Set(sel.value)
    next.has(entry.name) ? next.delete(entry.name) : next.add(entry.name)
    sel.value = next
  } else {
    sel.value = new Set([entry.name])
  }
}
/** SFTP → Shell：让远端交互式终端 `cd` 到指定目录，保持两侧 CWD 一致 */
function syncShellCwd(path: string) {
  const backendId = activeSession.value?.backendId
  if (!backendId) return
  void invoke('sftp_sync_cwd', { sessionId: backendId, path }).catch(() => {})
}

function onRowDblClick(side: Side, entry: FsEntry) {
  lastSide.value = side
  const sel = side === 'local' ? localSelected : remoteSelected
  if (entry.type === 'dir') {
    sel.value = new Set()
    if (side === 'local') {
      void loadLocal(joinPath(localPath.value, entry.name))
    } else {
      const next = joinPath(remotePath.value, entry.name)
      void loadRemote(next)
      syncShellCwd(next)
    }
  } else {
    sel.value = new Set([entry.name])
    if (side === 'local') onUpload()
    else onDownload()
  }
}

function goUp(side: 'local' | 'remote') {
  if (side === 'local') void loadLocal(parentPath(localPath.value))
  else {
    const next = parentPath(remotePath.value)
    void loadRemote(next)
    syncShellCwd(next)
  }
}
function gotoCrumb(side: 'local' | 'remote', path: string) {
  if (side === 'local') void loadLocal(path)
  else {
    void loadRemote(path)
    syncShellCwd(path)
  }
}
function changeSort(side: 'local' | 'remote', key: SortKey) {
  const s = side === 'local' ? localSort : remoteSort
  if (s.value.key === key) s.value.dir = (s.value.dir * -1) as 1 | -1
  else s.value = { key, dir: 1 }
}

/* 顶栏刷新：本地与远端（若已连接）同时重载当前目录 */
function refreshAll() {
  void loadLocal(localPath.value)
  void loadRemote(remotePath.value)
}

/* 远端目录跟随活动后端会话：建立连接 → 加载登录默认目录；
   断开（backendId 置空）或切换会话 → 清空残留，避免把上一台主机的文件展示给新会话 */
watch(() => activeSession.value?.backendId, (id) => {
  remoteRaw.value = []
  remotePath.value = '/'
  remoteSelected.value = new Set()
  remoteLoading.value = false
  remoteReqSeq++ // 使在飞的旧会话请求全部过期
  applySftpIdleTimeout(id)
  resetTree()
  // ssh:// 外部连接不自动列目录：那会另开一条 SFTP 通道（后端也会拒绝）
  if (id && !extTerminal.value) void loadRemote()
})

/* 远端目录一变：路径条回写、树上把这条路径摊开（历史在 loadRemote 成功处记） */
watch(remotePath, (p) => {
  pathDraft.value = p
  void revealInTree(p)
}, { immediate: true })

/** SFTP 通道空闲自动关闭阈值下发（秒，0 = 常驻不关）：
 *  行为设置单一事实源在前端，Rust 侧不读 app_config.json（见 ssh/sftp.rs SftpState） */
function applySftpIdleTimeout(id: string | undefined) {
  if (!id || !isTauri) return
  const sec = Math.max(0, Math.trunc(savedSettings.sftpIdleTimeoutSec) || 0)
  void invoke('sftp_set_idle_timeout', { sessionId: id, sec })
    .catch(e => reportLog('debug', SFTP_MANAGE_REMOVE, '设置 SFTP 空闲超时失败', { err: String(e) }))
}

// 改设置即刻对已建立的会话生效，无需重连
watch(() => savedSettings.sftpIdleTimeoutSec, () => applySftpIdleTimeout(activeSession.value?.backendId))

/* Shell → SFTP：终端执行 cd 后后端经 0x08 帧上报新 CWD，文件树同步跳转。
   仅当上报路径与当前远端目录不同时才刷新，避免 SFTP 导航触发的回环。 */
watch(activeSftpCwd, (cwd) => {
  if (!cwd || cwd === remotePath.value) return
  remotePath.value = cwd
  remoteSelected.value = new Set()
  void loadRemote(cwd)
})

/* ---- Dock tab 切换（状态在 store，供顶部工具栏等外部入口同步） ---- */
const activeTab = dockTab

/* 外部连接（堡垒机 ssh:// / sftp:// 链接）的通道门禁：
 * 一条连接只允许一条通道 —— terminal 会话的唯一通道是 PTY，SFTP 与端口转发
 * 都要另开通道，因而隐藏入口；sftp 会话反过来没有终端，SFTP 就是它的主视图 */
const extTerminal = computed(() => activeSession.value?.host.external === 'terminal')
const externalActive = computed(() => !!activeSession.value?.host.external)
watch([extTerminal, externalActive], ([terminal, external]) => {
  if (!external) return
  if (dockTab.value === 'sftp' && terminal) dockTab.value = 'log'
  if (dockTab.value === 'tunnel') dockTab.value = 'log'
}, { immediate: true })

function switchTab(tab: 'sftp' | 'log' | 'tunnel') {
  activeTab.value = tab
  expandDock() // 折叠状态下点击 tab 时一并展开
}

/* ---- 应用日志（数据源：stores/applog，后端 Hub 订阅推送；面板显示 = 磁盘文件内容） ----
   日志行模型 PanelLogEntry 由 store 提供（id=seq 全局唯一、time 已格式化、msg 含 kv 文本）。
   SFTP 队列事件经 report() 上报后端统一打 seq/ts，再由订阅回推显示，不经本地直插。 */

const LEVEL_FILTERS: Array<{ key: LogLevel | 'ALL'; label: string }> = [
  { key: 'ALL', label: '全部' },
  { key: 'INFO', label: 'INFO' },
  { key: 'WARN', label: 'WARN' },
  { key: 'ERROR', label: 'ERROR' },
  { key: 'DEBUG', label: 'DEBUG' },
]
const levelFilter = ref<LogLevel | 'ALL'>('ALL')
const filteredLogs = computed(() =>
  // 始终返回新数组：ALL 时也 slice()，否则原地 push 不改变引用，watch 不会触发
  levelFilter.value === 'ALL'
    ? logs.value.slice()
    : logs.value.filter(l => l.level === levelFilter.value),
)
const levelCounts = computed(() => {
  const c: Record<LogLevel, number> = { INFO: 0, WARN: 0, ERROR: 0, DEBUG: 0 }
  for (const l of logs.value) c[l.level]++
  return c
})

const autoScroll = ref(true)
// 日志自动换行（纯视图偏好，默认开启，随 ui_state 节持久化）；关闭时长行单行横向滚动。
// 本组件晚于启动快照挂载，初始值直接读快照；onConfigLoad 处理导入重载后的再 hydrate。
const logWrap = ref(getSnapshot()?.uiState.logWrap !== false)
let wrapHydrated = true
onConfigLoad(snap => {
  wrapHydrated = false
  logWrap.value = snap.uiState.logWrap !== false
  wrapHydrated = true
})
watch(
  logWrap,
  v => {
    if (!wrapHydrated) return
    patchUiState({ logWrap: v })
  },
  { flush: 'sync' },
)
const logListEl = ref<HTMLElement | null>(null)

const STICK_GAP = 24 // 距底部小于该值视为"贴底"
function isAtBottom() {
  const el = logListEl.value
  return !!el && el.scrollHeight - el.scrollTop - el.clientHeight < STICK_GAP
}
function scrollLogToBottom() {
  const el = logListEl.value
  if (el) el.scrollTop = el.scrollHeight
}
// 手动上滚查看历史 → 自动暂停跟随；滚回底部 → 自动恢复跟随
function onLogScroll() {
  autoScroll.value = isAtBottom()
}
// 点击按钮：跟随中 → 暂停跟随；暂停中 → 立即回到底部并恢复跟随
function toggleAutoScroll() {
  if (autoScroll.value) {
    autoScroll.value = false
  } else {
    autoScroll.value = true
    scrollLogToBottom()
  }
}
watch(filteredLogs, async () => {
  // 搜索状态下由匹配跳转控制滚动位置，不强制贴底
  if (activeTab.value === 'log' && autoScroll.value && !searchActive.value) {
    await nextTick()
    scrollLogToBottom()
  }
})
watch(activeTab, async (tab) => {
  if (tab === 'log') {
    await nextTick()
    // 切回日志 tab 时按当前跟随状态决定位置
    if (autoScroll.value) scrollLogToBottom()
  }
})
function clearLogs() {
  // 清空后端内存缓冲 + 前端显示（磁盘文件不受影响）
  void clearAppLogs()
}

/* ---- 日志搜索（关键字高亮 + 上/下匹配跳转，可与级别筛选叠加） ---- */
const searchVisible = ref(false)
const keyword = ref('')
const caseSensitive = ref(false)
const currentMatchIdx = ref(0)
const searchInputEl = ref<HTMLInputElement | null>(null)

const searchActive = computed(() => searchVisible.value && keyword.value.length > 0)

function matchText(msg: string) {
  const kw = keyword.value
  if (!kw) return false
  return caseSensitive.value ? msg.includes(kw) : msg.toLowerCase().includes(kw.toLowerCase())
}

// 当前级别筛选结果内的全部匹配行 id
const matchIds = computed(() =>
  searchActive.value ? filteredLogs.value.filter(l => matchText(l.msg)).map(l => l.id) : [],
)
const currentMatchId = computed(() => matchIds.value[currentMatchIdx.value] ?? null)

async function openSearch() {
  searchVisible.value = true
  await nextTick()
  searchInputEl.value?.focus()
  searchInputEl.value?.select()
}
function closeSearch() {
  searchVisible.value = false
}
// Ctrl/⌘+F 全局唤起搜索（自动切到日志 tab 并展开面板）
function revealSearch() {
  activeTab.value = 'log'
  expandDock()
  openSearch()
}
function onFindShortcut(e: KeyboardEvent) {
  if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'f') {
    e.preventDefault()
    revealSearch()
  }
}
// 标题栏搜索按钮通过 searchTick 触发
watch(searchTick, revealSearch)
function gotoMatch(dir: 1 | -1) {
  const n = matchIds.value.length
  if (!n) return
  currentMatchIdx.value = (currentMatchIdx.value + dir + n) % n
}

// 关键字 / 大小写变化：回到第一个匹配；匹配数减少时收敛索引
watch([keyword, caseSensitive], () => { currentMatchIdx.value = 0 })
watch(matchIds, (ids) => {
  if (currentMatchIdx.value > ids.length - 1) {
    currentMatchIdx.value = Math.max(0, ids.length - 1)
  }
})
// 当前匹配行滚入可视区域
watch([currentMatchId, searchVisible], async () => {
  if (!searchActive.value) return
  await nextTick()
  logListEl.value
    ?.querySelector('.log-row.match-current')
    ?.scrollIntoView({ block: 'nearest' })
})

// 将消息文本按匹配位置拆分为片段（不用 v-html，避免日志内容 XSS）
function highlightParts(msg: string): Array<{ text: string; hit: boolean }> {
  const kw = keyword.value
  if (!searchActive.value || !kw) return [{ text: msg, hit: false }]
  const hay = caseSensitive.value ? msg : msg.toLowerCase()
  const needle = caseSensitive.value ? kw : kw.toLowerCase()
  const parts: Array<{ text: string; hit: boolean }> = []
  let i = 0
  let p = hay.indexOf(needle)
  while (p !== -1) {
    if (p > i) parts.push({ text: msg.slice(i, p), hit: false })
    parts.push({ text: msg.slice(p, p + kw.length), hit: true })
    i = p + kw.length
    p = hay.indexOf(needle, i)
  }
  if (i < msg.length) parts.push({ text: msg.slice(i), hit: false })
  return parts
}

/* resizer 拖拽 */
const dockEl = ref<HTMLElement | null>(null)
const isResizing = ref(false)
let dragging = false
let pendingY = 0
let rafId = 0

/* ---- SFTP 双栏分割拖拽（本地/远程宽度占比，持久化在 store.sftpLocalRatio） ---- */
const isSplitting = ref(false)
let splitting = false
let splitStartX = 0
let splitStartRatio = 0.5
let splitUsableW = 0   // sftp-main 宽度 - 中间传输列宽度
let pendingX = 0
let splitRaf = 0

function startSplitDrag(e: MouseEvent) {
  e.preventDefault()
  const mainEl = (e.target as HTMLElement).closest('.sftp-main') as HTMLElement | null
  if (!mainEl) return
  splitting = true
  isSplitting.value = true
  splitStartX = e.clientX
  pendingX = e.clientX // 初始化，防止单击时 onUp 收尾用残留值计算 dx
  splitStartRatio = sftpLocalRatio.value
  // 占比只作用于「本机栏 + 远程栏」，目录树那段宽度不参与均分
  splitUsableW = mainEl.getBoundingClientRect().width - sftpTreeWidth.value
  document.body.style.userSelect = 'none'
  document.body.style.cursor = 'col-resize'
}

/** 双击恢复均分 */
function onSplitDblClick(e: MouseEvent) {
  e.preventDefault()
  sftpLocalRatio.value = 0.5
}

function applySplit() {
  splitRaf = 0
  if (!splitUsableW) return
  const dx = pendingX - splitStartX
  sftpLocalRatio.value = Math.min(0.8, Math.max(0.2, splitStartRatio + dx / splitUsableW))
}

function endSplitDrag() {
  splitting = false
  isSplitting.value = false
  if (splitRaf) { cancelAnimationFrame(splitRaf); splitRaf = 0 }
  if (!dragging) {
    document.body.style.userSelect = ''
    document.body.style.cursor = ''
  }
}

const HEAD_H = 40        // 标题栏高度（与 CSS .dock-head / .dock.collapsed 保持一致），拖拽下限
const MIN_H = 120        // 视为"可用"的最小高度，低于此展开时自动恢复
const COLLAPSE_GAP = 48  // 触底后继续下拉该距离 → 自动折叠，并记住可用高度
const EXPAND_GAP = 24    // 折叠态上拉超过该距离 → 展开；折叠/展开在同一次拖拽内可反复切换
// 可用高度统一存于全局 dockHeight（折叠态下保留，展开/重启无缝恢复）

/** 实时几何：以 workspace / tabs 的真实矩形为基准，避免硬编码
 *  标题栏/Tab/状态栏高度常量（边框、macOS 安全区造成 1~2px 偏差即出现顶缝） */
function geometry() {
  const el = dockEl.value
  const ws = el?.parentElement
  if (!el || !(ws instanceof HTMLElement)) return null
  const wsRect = ws.getBoundingClientRect()
  const tabsEl = ws.querySelector(':scope > .tabs')
  const tabsBottom = tabsEl?.getBoundingClientRect().bottom ?? wsRect.top
  return { wsBottom: wsRect.bottom, tabsBottom }
}

function startDrag(e: MouseEvent) {
  dragging = true
  isResizing.value = true
  pendingY = e.clientY
  if (!dockCollapsed.value) {
    // 从真实渲染高度同步（内联绑定值通常即此值，保险一次）
    const cur = dockEl.value?.offsetHeight ?? 0
    if (cur >= MIN_H) dockHeight.value = cur
  }
  document.body.style.userSelect = 'none'
  document.body.style.cursor = 'ns-resize'
}

function endDrag() {
  dragging = false
  isResizing.value = false
  if (rafId) {
    cancelAnimationFrame(rafId)
    rafId = 0
  }
  if (!splitting) {
    document.body.style.userSelect = ''
    document.body.style.cursor = ''
  }
}

function applyHeight() {
  rafId = 0
  const g = geometry()
  if (!g) return
  // 期望高度 = workspace 底边 - 鼠标Y；上限 = workspace 底边 - Tab 栏真实底边
  const h = g.wsBottom - pendingY
  const max = g.wsBottom - g.tabsBottom

  if (!dockCollapsed.value) {
    // 已到下限（保留标题栏）仍继续下拉超过阈值 → 自动折叠。
    // 注意：不结束拖拽手势，反向拉回超过展开阈值可在同一次拖拽内立即重新展开
    if (h <= HEAD_H - COLLAPSE_GAP) {
      dockCollapsed.value = true // dockHeight 保留，展开无缝恢复
      return
    }
    const clamped = Math.min(Math.max(h, HEAD_H), max)
    dockHeight.value = clamped
    return
  }

  // 折叠态：上拉超过阈值 → 展开，高度继续实时跟随鼠标
  if (h >= HEAD_H + EXPAND_GAP) {
    dockCollapsed.value = false
    dockHeight.value = Math.min(Math.max(h, HEAD_H), max)
  }
}

function onMove(e: MouseEvent) {
  if (splitting) {
    pendingX = e.clientX
    if (!splitRaf) splitRaf = requestAnimationFrame(applySplit)
  }
  if (treeSplitting) {
    treePendingX = e.clientX
    if (!treeRaf) treeRaf = requestAnimationFrame(applyTreeDrag)
  }
  if (!dragging) return
  pendingY = e.clientY
  if (!rafId) rafId = requestAnimationFrame(applyHeight)
}
function onUp() {
  if (splitting) {
    endSplitDrag()
    applySplit() // 收尾：应用最后一次 pendingX
  }
  if (treeSplitting) {
    endTreeDrag()
    applyTreeDrag()
  }
  if (!dragging) return
  endDrag()
  applyHeight() // 收尾：应用最后一次 pendingY
}

/* 展开 / 折叠 */
function expandDock() {
  if (!dockCollapsed.value) return
  if (dockHeight.value < MIN_H) dockHeight.value = DOCK_DEFAULT_HEIGHT
  dockCollapsed.value = false
}

function toggleDock() {
  if (dockCollapsed.value) expandDock()
  else dockCollapsed.value = true
}
/* ---- SFTP 快捷键：作用于最后交互的一侧；右键菜单打开/输入框聚焦时不介入 ---- */
function onSftpShortcut(e: KeyboardEvent) {
  if (!dockVisible.value || activeTab.value !== 'sftp') return
  if (menu.value) return // 菜单有自己的键盘行为（Esc/方向/Enter）
  const el = e.target as HTMLElement | null
  if (el && (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA')) return
  const side = lastSide.value
  const meta = e.metaKey || e.ctrlKey
  const key = e.key.toLowerCase()

  if (meta && key === 'c') {
    e.preventDefault()
    copySelection(side, 'copy')
  } else if (meta && key === 'x') {
    e.preventDefault()
    copySelection(side, 'cut')
  } else if (meta && key === 'v') {
    e.preventDefault()
    void pasteTargets(side)
  } else if (meta && key === 'a') {
    e.preventDefault()
    selectAll(side)
  } else if (e.key === 'F2') {
    const t = selectedEntries(side)
    if (t.length === 1) startEdit(side, 'rename', t[0])
  } else if (e.key === 'Delete' || e.key === 'Backspace') {
    const t = selectedEntries(side)
    if (t.length) void deleteTargets(side, t)
  } else if (meta && key === 'enter') {
    // ⌘↵ 对应右键菜单的「用编辑器打开」（远端文件，支持多选批量）
    const t = selectedEntries(side)
    if (t.length) void openInEditor(t)
  } else if (e.key === 'Enter') {
    const t = selectedEntries(side)
    if (t.length === 1) onRowDblClick(side, t[0]!)
  }
}

onMounted(() => {
  // 订阅后端应用日志流（replay + 增量；幂等，多订阅者广播）
  subscribeAppLogs()
  // 首次打开加载本地目录（不传路径 → 后端解析为用户主目录）
  void loadLocal()
  // 组件重挂时活动会话可能早已在线（watch 不会补发），主动加载一次远端默认目录
  if (activeSession.value?.backendId) resetTree()
  if (activeSession.value?.backendId && !extTerminal.value) void loadRemote()

  // 视口适配：上次保存的高度可能超出当前窗口（换显示器/全屏化），
  // 以 workspace / tabs 真实矩形夹取，上限即 Tab 下沿
  const g = geometry()
  if (g) {
    const maxH = g.wsBottom - g.tabsBottom
    if (maxH >= HEAD_H) dockHeight.value = Math.min(dockHeight.value, maxH)
    if (dockHeight.value < HEAD_H) dockHeight.value = DOCK_DEFAULT_HEIGHT
  }

  document.addEventListener('mousemove', onMove)
  document.addEventListener('mouseup', onUp)
  document.addEventListener('keydown', onFindShortcut)
  document.addEventListener('keydown', onSftpShortcut)

  // 拖拽上传：路径由 webview 级拖放事件给出（多选文件/多选文件夹一次全给），
  // 只在落点位于文件栏主体内时建任务——落在 AI 聊天附件区则归那边处理，不劫持成上传。
  // 两条分支各记一条日志：「事件没到」和「落点没命中」在界面上是同一个没反应，
  // 只有把落点与区域矩形一起落盘才能分辨。
  dropStop = watchFileDrop(dropZone, (inside) => { dropHint.value = inside }, paths => {
    reportLog('info', SFTP_DROP_ACCEPTED, '拖入文件栏，按拖入项建上传任务', {
      name: baseNameOf(paths[0] ?? ''), count: paths.length, dir: 'up', to: remotePath.value,
    })
    void uploadLocalPaths(paths)
  }, (paths, pos) => {
    // 文件栏没显示（页签切走/未连接）时 zone 为空，本就谈不上命中，不记
    const el = dropZone()
    if (!el) return
    const r = el.getBoundingClientRect()
    const c = dropToCss(pos)
    reportLog('info', SFTP_DROP_MISSED, '拖入落点在文件栏主体之外，未建任务', {
      name: baseNameOf(paths[0] ?? ''), count: paths.length, dir: 'up',
      pos: `${Math.round(pos.x)},${Math.round(pos.y)}`,
      css: `${Math.round(c.x)},${Math.round(c.y)}`,
      zone: `${Math.round(r.left)},${Math.round(r.top)} → ${Math.round(r.right)},${Math.round(r.bottom)}`,
      os: detectPlatform(), dpr: window.devicePixelRatio || 1,
    })
  })
})
onUnmounted(() => {
  dropStop?.()
  dropStop = null
  if (editTimer) { clearInterval(editTimer); editTimer = null }
  document.removeEventListener('mousemove', onMove)
  document.removeEventListener('mouseup', onUp)
  document.removeEventListener('keydown', onFindShortcut)
  document.removeEventListener('keydown', onSftpShortcut)
  // 浮层仍打开时卸载：watcher 不会触发，这里兜底摘掉 document 监听
  document.removeEventListener('mousedown', onQueueDismiss, true)
  document.removeEventListener('keydown', onQueueDismiss, true)
  document.removeEventListener('mousedown', onHistoryDismiss, true)
  document.removeEventListener('keydown', onHistoryDismiss, true)
})
</script>

<template>
  <section ref="dockEl" class="dock" :class="{ collapsed: dockCollapsed, resizing: isResizing }"
           :style="{ height: dockHeight + 'px' }">
    <div class="dock-resizer" title="拖拽调整高度（下拉到底折叠，折叠时上拉展开）· 双击折叠 / 展开"
         @mousedown.prevent="startDrag" @dblclick="toggleDock"></div>

    <div class="dock-head">

      <button class="dock-tab" :class="{ active: activeTab === 'log' }" title="日志"
              @click="switchTab('log')">
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
             stroke-linecap="round" stroke-linejoin="round">
          <polyline points="4 17 10 11 4 5"></polyline>
          <line x1="12" y1="19" x2="20" y2="19"></line>
        </svg>
        日志
      </button>

      <button v-if="!externalActive" class="dock-tab" :class="{ active: activeTab === 'tunnel' }" title="端口转发"
              @click="switchTab('tunnel')">
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
             stroke-linecap="round" stroke-linejoin="round">
          <circle cx="5" cy="12" r="2.5"></circle>
          <circle cx="19" cy="12" r="2.5"></circle>
          <path d="M7.5 12h9"></path>
        </svg>
        端口转发
      </button>

      <button v-if="!extTerminal" class="dock-tab" :class="{ active: activeTab === 'sftp' }" title="文件管理"
              @click="switchTab('sftp')">
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
             stroke-linecap="round" stroke-linejoin="round">
          <path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"></path>
        </svg>
        文件
      </button>

      <div class="spacer"></div>

      <button class="dock-toggle" title="收起 / 展开" @click="toggleDock">
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2"
             stroke-linecap="round" stroke-linejoin="round">
          <polyline points="6 9 12 15 18 9"></polyline>
        </svg>
      </button>

      <button class="dock-toggle" title="关闭面板" @click="closeDock">
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2"
             stroke-linecap="round" stroke-linejoin="round">
          <line x1="18" y1="6" x2="6" y2="18"></line>
          <line x1="6" y1="6" x2="18" y2="18"></line>
        </svg>
      </button>
    </div>

    <div class="dock-body">
      <!-- SFTP -->
      <div class="dock-pane" :class="{ show: activeTab === 'sftp' }">
        <div class="sftp">
          <!-- 顶栏：左=当前远程目录（可输入跳转），中=工具组，右=下载落点与传输记录 -->
          <div class="sftp-topbar">
            <div class="sftp-path">
              <span class="led" :class="{ off: !connected }"></span>
              <input
                v-model="pathDraft"
                class="path-input"
                type="text"
                spellcheck="false"
                autocomplete="off"
                placeholder="/"
                :disabled="!connected || !isTauri"
                :title="connected ? '当前远程目录；输入绝对路径后回车跳转' : '未连接到远程主机'"
                @keydown.enter.prevent="commitPath"
                @keydown.esc.prevent="pathDraft = remotePath"
                @blur="pathDraft = remotePath"
              />
              <span class="path-host" :class="{ off: !connected }">{{ connected ? activeHost?.id : '未连接' }}</span>
            </div>

            <div class="sftp-tools">
              <div class="tool-menu">
                <button class="sftp-btn" :class="{ on: historyOpen }" title="本次连接内访问过的远程目录"
                        @click="historyOpen = !historyOpen">
                  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
                       stroke-linecap="round" stroke-linejoin="round">
                    <path d="M3 12a9 9 0 109-9"/><path d="M3 4v5h5"/><path d="M12 7v5l3 2"/>
                  </svg>
                  历史
                </button>
                <div v-if="historyOpen" class="tool-pop">
                  <div v-if="!remoteHistory.length" class="tool-pop-empty">暂无访问记录</div>
                  <button v-for="p in remoteHistory" :key="p" class="tool-pop-item"
                          :class="{ now: p === remotePath }" @click="pickHistory(p)">{{ p }}</button>
                </div>
              </div>
              <button class="sftp-btn" title="刷新本地与远端目录" @click="refreshAll">
                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
                     stroke-linecap="round" stroke-linejoin="round">
                  <path d="M21 12a9 9 0 11-3-6.7L21 8"/><path d="M21 3v5h-5"/>
                </svg>
                刷新
              </button>
              <button class="sftp-btn" title="上一级目录" :disabled="!connected" @click="goUp('remote')">
                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
                     stroke-linecap="round" stroke-linejoin="round">
                  <path d="M19 12H5M12 19l-7-7 7-7"/>
                </svg>
                上一级
              </button>
              <button class="sftp-btn"
                      :title="`选择本机文件上传到 ${remotePath}`"
                      :disabled="!connected" @click="onUploadOrPicker">
                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
                     stroke-linecap="round" stroke-linejoin="round">
                  <path d="M12 19V5M5 12l7-7 7 7"/>
                </svg>
                上传
              </button>
              <button class="sftp-btn" :class="{ ready: remoteSel.length }"
                      :title="remoteSel.length ? `下载选中的 ${remoteSel.length} 个文件` : '先在文件列表中选择文件'"
                      :disabled="!remoteSel.length" @click="onDownload">
                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
                     stroke-linecap="round" stroke-linejoin="round">
                  <path d="M12 5v14M5 12l7 7 7-7"/>
                </svg>
                下载
              </button>
              <button class="sftp-btn" title="在当前远程目录新建文件夹" :disabled="!connected"
                      @click="startEdit('remote', 'mkdir')">
                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
                     stroke-linecap="round" stroke-linejoin="round">
                  <path d="M4 7a2 2 0 012-2h3l2 2h7a2 2 0 012 2v8a2 2 0 01-2 2H6a2 2 0 01-2-2V7z"/>
                  <path d="M12 11v5M9.5 13.5h5" stroke-width="2.2"/>
                </svg>
                新建
              </button>
            </div>

            <div class="sftp-side right">
              <!-- 本机栏隐藏时（默认）下载落点没有别的入口，这里直接给出并可更换 -->
              <button v-if="!showLocalPane" class="sftp-btn"
                      :title="`下载保存目录：${localPath}（点击更换本机目录）`"
                      @click="pickDownloadDir">
                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
                     stroke-linecap="round" stroke-linejoin="round">
                  <path d="M3 7a2 2 0 012-2h4l2 2h8a2 2 0 012 2v8a2 2 0 01-2 2H5a2 2 0 01-2-2V7z"/>
                  <path d="M12 10v5M9.5 13l2.5 2.5L14.5 13"/>
                </svg>
                下载到 {{ shortLocal(localPath) }}
              </button>
              <!-- 传输记录：点击弹出右上角浮层（不占正文空间）；按钮常驻显示活动指示灯、
                   任务数与一行进度摘要，收起时也看得到进行中/失败/完成 -->
              <button class="sftp-btn queue-trigger" :class="{ on: queueOpen }"
                      :title="queueSummary ? `传输记录：${queueSummary}` : '传输记录'"
                      @click="toggleQueue">
                <span class="queue-led" :class="{ idle: !runningCount && !pendingCount }"></span>
                传输记录
                <span class="queue-badge">{{ transfers.length }}</span>
                <span v-if="queueSummary" class="queue-summary">{{ queueSummary }}</span>
              </button>
            </div>
          </div>

          <!-- 主体：左=远程目录树，右=文件列表（本机栏开启时插在中间） -->
          <div ref="sftpMainEl" class="sftp-main" :class="{ splitting: isSplitting || isTreeSplitting }"
               :style="{ '--lr': sftpLocalRatio, '--lrw': `calc(${sftpTreeWidth}px + (100% - ${sftpTreeWidth}px) * ${sftpLocalRatio})` }">
            <!-- 远程目录树：懒加载，只列目录；点行同时切右侧列表并展开该层 -->
            <div class="file-tree" :style="{ flex: `0 0 ${sftpTreeWidth}px` }">
              <div class="tree-head">
                <span>目录</span>
                <button class="tree-refresh" title="重新加载已展开的目录" :disabled="!connected"
                        @click="reloadTree">
                  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
                       stroke-linecap="round" stroke-linejoin="round">
                    <path d="M21 12a9 9 0 11-3-6.7L21 8"/><path d="M21 3v5h-5"/>
                  </svg>
                </button>
              </div>
              <div class="tree-scroll">
                <div v-if="!connected" class="tree-empty">未连接到远程主机</div>
                <div
                  v-for="r in treeRows"
                  :key="r.path"
                  class="tree-row"
                  :class="{ active: r.path === remotePath, failed: r.failed }"
                  :style="{ paddingLeft: `${3 + r.depth * 11}px` }"
                  :title="r.path"
                  @click="onTreeRowClick(r)"
                >
                  <button
                    class="tree-twisty"
                    :class="{ open: r.expanded, loading: r.loading }"
                    :title="r.loading ? '加载中…' : r.expanded ? '收起' : '展开'"
                    @click.stop="toggleTree(r.path)"
                  ><svg v-if="!r.loading" viewBox="0 0 16 16"><path d="M6 4l4 4-4 4" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/></svg><span v-else class="tree-spin"></span></button>
                  <svg class="tree-icon" viewBox="0 0 24 24">
                    <path d="M4 7a2 2 0 012-2h3l2 2h7a2 2 0 012 2v8a2 2 0 01-2 2H6a2 2 0 01-2-2V7z"/>
                  </svg>
                  <span class="tree-name">{{ r.path === '/' ? '/' : r.name }}</span>
                </div>
              </div>
            </div>
            <!-- 树宽分割线：拖拽调宽，双击恢复默认（宽度持久化在 store.sftpTreeWidth） -->
            <div class="tree-divider" :class="{ dragging: isTreeSplitting }"
                 :style="{ left: `${sftpTreeWidth}px` }"
                 title="拖拽调整目录树宽度，双击恢复默认"
                 @mousedown="startTreeDrag"
                 @dblclick="onTreeDblClick"></div>
            <!-- 本地 -->
            <div v-if="showLocalPane" class="pane local"
                 :style="{ flex: `0 0 calc((100% - ${sftpTreeWidth}px) * ${sftpLocalRatio})` }">
              <div class="pane-header">
                <span class="pane-tag">本地</span>
                <div class="pane-path">
                  <template v-for="(c, i) in crumbs(localPath)" :key="c.path">
                    <span v-if="i" class="sep">/</span>
                    <span class="crumb" @click="gotoCrumb('local', c.path)">{{ c.label }}</span>
                  </template>
                </div>
                <button class="pane-btn" title="上一级" @click="goUp('local')">
                  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
                       stroke-linecap="round" stroke-linejoin="round">
                    <path d="M19 12H5M12 19l-7-7 7-7"/>
                  </svg>
                </button>
                <button class="pane-btn" title="刷新" :disabled="localLoading" @click="loadLocal(localPath)">
                  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
                       stroke-linecap="round" stroke-linejoin="round">
                    <path d="M21 12a9 9 0 11-3-6.7L21 8"/><path d="M21 3v5h-5"/>
                  </svg>
                </button>
                <button class="pane-btn" title="新建文件夹" @click="startEdit('local', 'mkdir')">
                  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
                       stroke-linecap="round" stroke-linejoin="round">
                    <path d="M4 7a2 2 0 012-2h3l2 2h7a2 2 0 012 2v8a2 2 0 01-2 2H6a2 2 0 01-2-2V7z"/>
                    <path d="M12 11v5M9.5 13.5h5" stroke-width="2.2"/>
                  </svg>
                </button>
                <!-- 导航区 | 传输区 -->
                <span class="sep"></span>
                <button class="pane-btn transfer-btn" :class="{ ready: localSel.length }"
                        :title="localSel.length ? `上传选中的 ${localSel.length} 个文件到远程` : '先在列表中选择文件'"
                        :disabled="!localSel.length"
                        @click="onUpload">
                  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
                       stroke-linecap="round" stroke-linejoin="round">
                    <path d="M12 19V5M5 12l7-7 7 7"/>
                  </svg>
                </button>
              </div>
              <div class="list-header">
                <div class="lh-name" :class="{ sorted: localSort.key==='name', asc: localSort.dir===1, desc: localSort.dir===-1 }" @click="changeSort('local','name')">文件名</div>
                <div class="lh-size" :class="{ sorted: localSort.key==='size' }" @click="changeSort('local','size')">大小</div>
                <div class="lh-kind">类型</div>
                <div class="lh-time" :class="{ sorted: localSort.key==='mtime' }" @click="changeSort('local','mtime')">修改时间</div>
                <div class="lh-perm">权限</div>
                <div class="lh-user">用户</div>
                <div class="lh-group">用户组</div>
              </div>
              <div class="pane-list" @contextmenu="onBlankContext($event, 'local')">
                <div v-if="!localEntries.length && !(edit && edit.side==='local' && edit.kind!=='rename')" class="pane-empty">
                  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5"
                       stroke-linecap="round" stroke-linejoin="round">
                    <path d="M4 7a2 2 0 012-2h3l2 2h7a2 2 0 012 2v8a2 2 0 01-2 2H6a2 2 0 01-2-2V7z"/>
                  </svg>
                  <div>{{ localLoading ? '加载中…' : '此文件夹为空' }}</div>
                </div>
                <template v-else>
                  <div
                    v-for="e in localEntries"
                    :key="e.name"
                    class="row"
                    :class="{ dir: e.type==='dir', selected: localSelected.has(e.name) }"
                    @click="onRowClick('local', e, $event)"
                    @dblclick="onRowDblClick('local', e)"
                    @contextmenu.stop="onRowContext($event, 'local', e)"
                  >
                    <div class="row-name">
                      <svg class="row-icon" viewBox="0 0 24 24">
                        <path v-if="e.type==='dir'" d="M4 7a2 2 0 012-2h3l2 2h7a2 2 0 012 2v8a2 2 0 01-2 2H6a2 2 0 01-2-2V7z"/>
                        <template v-else>
                          <path d="M14 3H6a2 2 0 00-2 2v14a2 2 0 002 2h12a2 2 0 002-2V9z"/>
                          <path d="M14 3v6h6"/>
                        </template>
                      </svg>
                      <!-- 重命名：原位行内编辑；其它情况纯文本 -->
                      <input v-if="edit && edit.side==='local' && edit.kind==='rename' && edit.origName===e.name"
                             ref="editInput" v-model="editName" class="rename-input"
                             @keydown.enter.prevent="confirmEdit"
                             @keydown.esc.prevent="cancelEdit"
                             @blur="confirmEdit" />
                      <span v-else class="row-text">{{ e.name }}</span>
                      <svg v-if="e.isSymlink" class="link-badge" viewBox="0 0 16 16" title="符号链接">
                        <path d="M7 2H4a3 3 0 0 0 0 6h2" stroke="currentColor" stroke-width="1.8" fill="none" stroke-linecap="round"/>
                        <path d="M9 14h3a3 3 0 0 0 0-6h-2" stroke="currentColor" stroke-width="1.8" fill="none" stroke-linecap="round"/>
                      </svg>
                    </div>
                    <div class="row-size">{{ e.type==='dir' ? '—' : fmtSize(e.size) }}</div>
                    <div class="row-kind">{{ kindLabel(e) }}</div>
                    <div class="row-time">{{ e.mtime }}</div>
                    <div class="row-perm">{{ e.perm }}</div>
                    <div class="row-user">{{ e.ownerUser }}</div>
                    <div class="row-group">{{ e.ownerGroup }}</div>
                  </div>
                  <!-- 新建文件夹/文件编辑行（列表底部，不隐藏已有文件） -->
                  <div v-if="edit && edit.side==='local' && (edit.kind==='mkdir' || edit.kind==='mkfile')"
                       class="row mkdir-row">
                    <div class="row-name">
                      <svg class="row-icon" viewBox="0 0 24 24">
                        <path v-if="edit.kind==='mkdir'" d="M4 7a2 2 0 012-2h3l2 2h7a2 2 0 012 2v8a2 2 0 01-2 2H6a2 2 0 01-2-2V7z"/>
                        <template v-else>
                          <path d="M14 3H6a2 2 0 00-2 2v14a2 2 0 002 2h12a2 2 0 002-2V9z"/>
                          <path d="M14 3v6h6"/>
                        </template>
                      </svg>
                      <input ref="editInput" v-model="editName" class="mkdir-input"
                             :placeholder="edit.kind==='mkdir' ? '输入文件夹名，回车确认' : '输入文件名，回车确认'"
                             @keydown.enter.prevent="confirmEdit"
                             @keydown.esc.prevent="cancelEdit"
                             @blur="confirmEdit" />
                    </div>
                  </div>
                </template>
              </div>
              <div class="pane-stats">
                <b>{{ localEntries.filter(e=>e.type==='dir').length }}</b> 目录 /
                <b>{{ localEntries.filter(e=>e.type==='file').length }}</b> 文件
                <template v-if="localSelected.size"> · 选中 {{ localSelected.size }}</template>
              </div>
            </div>

            <!-- 远程：导航（路径条/上一级/新建/上传下载）统一在顶栏，这里只有表头与列表 -->
            <div class="pane remote">
              <div class="list-header">
                <div class="lh-name" :class="{ sorted: remoteSort.key==='name', asc: remoteSort.dir===1, desc: remoteSort.dir===-1 }" @click="changeSort('remote','name')">文件名</div>
                <div class="lh-size" :class="{ sorted: remoteSort.key==='size' }" @click="changeSort('remote','size')">大小</div>
                <div class="lh-kind">类型</div>
                <div class="lh-time" :class="{ sorted: remoteSort.key==='mtime' }" @click="changeSort('remote','mtime')">修改时间</div>
                <div class="lh-perm">权限</div>
                <div class="lh-user">用户</div>
                <div class="lh-group">用户组</div>
              </div>
              <div class="pane-list" @contextmenu="onBlankContext($event, 'remote')">
                <div v-if="!remoteEntries.length && !(edit && edit.side==='remote' && edit.kind!=='rename')" class="pane-empty">
                  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5"
                       stroke-linecap="round" stroke-linejoin="round">
                    <path d="M4 7a2 2 0 012-2h3l2 2h7a2 2 0 012 2v8a2 2 0 01-2 2H6a2 2 0 01-2-2V7z"/>
                  </svg>
                  <div>{{ !connected ? '未连接到远程主机' : remoteLoading ? '加载中…' : '此文件夹为空' }}</div>
                </div>
                <template v-else>
                  <div
                    v-for="e in remoteEntries"
                    :key="e.name"
                    class="row"
                    :class="{ dir: e.type==='dir', selected: remoteSelected.has(e.name) }"
                    @click="onRowClick('remote', e, $event)"
                    @dblclick="onRowDblClick('remote', e)"
                    @contextmenu.stop="onRowContext($event, 'remote', e)"
                  >
                    <div class="row-name">
                      <svg class="row-icon" viewBox="0 0 24 24">
                        <path v-if="e.type==='dir'" d="M4 7a2 2 0 012-2h3l2 2h7a2 2 0 012 2v8a2 2 0 01-2 2H6a2 2 0 01-2-2V7z"/>
                        <template v-else>
                          <path d="M14 3H6a2 2 0 00-2 2v14a2 2 0 002 2h12a2 2 0 002-2V9z"/>
                          <path d="M14 3v6h6"/>
                        </template>
                      </svg>
                      <input v-if="edit && edit.side==='remote' && edit.kind==='rename' && edit.origName===e.name"
                             ref="editInput" v-model="editName" class="rename-input"
                             @keydown.enter.prevent="confirmEdit"
                             @keydown.esc.prevent="cancelEdit"
                             @blur="confirmEdit" />
                      <span v-else class="row-text">{{ e.name }}</span>
                      <svg v-if="e.isSymlink" class="link-badge" viewBox="0 0 16 16" title="符号链接">
                        <path d="M7 2H4a3 3 0 0 0 0 6h2" stroke="currentColor" stroke-width="1.8" fill="none" stroke-linecap="round"/>
                        <path d="M9 14h3a3 3 0 0 0 0-6h-2" stroke="currentColor" stroke-width="1.8" fill="none" stroke-linecap="round"/>
                      </svg>
                    </div>
                    <div class="row-size">{{ e.type==='dir' ? '—' : fmtSize(e.size) }}</div>
                    <div class="row-kind">{{ kindLabel(e) }}</div>
                    <div class="row-time">{{ e.mtime }}</div>
                    <div class="row-perm">{{ e.perm }}</div>
                    <div class="row-user">{{ e.ownerUser }}</div>
                    <div class="row-group">{{ e.ownerGroup }}</div>
                  </div>
                  <!-- 新建文件夹/文件编辑行（列表底部，不隐藏已有文件） -->
                  <div v-if="edit && edit.side==='remote' && (edit.kind==='mkdir' || edit.kind==='mkfile')"
                       class="row mkdir-row">
                    <div class="row-name">
                      <svg class="row-icon" viewBox="0 0 24 24">
                        <path v-if="edit.kind==='mkdir'" d="M4 7a2 2 0 012-2h3l2 2h7a2 2 0 012 2v8a2 2 0 01-2 2H6a2 2 0 01-2-2V7z"/>
                        <template v-else>
                          <path d="M14 3H6a2 2 0 00-2 2v14a2 2 0 002 2h12a2 2 0 002-2V9z"/>
                          <path d="M14 3v6h6"/>
                        </template>
                      </svg>
                      <input ref="editInput" v-model="editName" class="mkdir-input"
                             :placeholder="edit.kind==='mkdir' ? '输入文件夹名，回车确认' : '输入文件名，回车确认'"
                             @keydown.enter.prevent="confirmEdit"
                             @keydown.esc.prevent="cancelEdit"
                             @blur="confirmEdit" />
                    </div>
                  </div>
                </template>
              </div>
              <div class="pane-stats">
                <b>{{ remoteEntries.filter(e=>e.type==='dir').length }}</b> 目录 /
                <b>{{ remoteEntries.filter(e=>e.type==='file').length }}</b> 文件
                <template v-if="remoteSelected.size"> · 选中 {{ remoteSelected.size }}</template>
              </div>
            </div>

            <!-- 分割线：绝对定位，不占用 flex 空间，hover/拖拽高亮，双击均分（本机栏隐藏时无从调整） -->
            <div v-if="showLocalPane" class="pane-divider" :class="{ dragging: isSplitting }"
                 title="拖拽调整宽度，双击恢复均分"
                 @mousedown="startSplitDrag"
                 @dblclick="onSplitDblClick">
              <div class="pane-divider-grip"></div>
            </div>
            <!-- 拖拽上传提示层：落点固定是「远端当前目录」，所以整块文件区都是接受区，
                 只在正拖着文件时出现，不影响平时布局与命中 -->
            <div v-if="dropHint" class="drop-hint">
              <div class="drop-hint-card">
                <span class="drop-hint-title">松开即上传（文件与文件夹，可多选）</span>
                <span class="drop-hint-path">{{ remotePath }}</span>
              </div>
            </div>
          </div>

          <!-- 传输记录浮层：锚在顶栏按钮正下方，覆盖文件区右上角，关闭后不占任何空间。
               头部只留批量操作，收起/展开改由顶栏按钮与右上角 ✕ 控制 -->
          <div v-show="queueOpen" class="queue-pop">
            <!-- 头部：指示灯 + 标题 + 总数；右侧 全部暂停 / 全部继续 / 清空 / 关闭 -->
            <div class="queue-head">
              <span class="queue-led" :class="{ idle: !runningCount && !pendingCount }"></span>
              <span class="queue-title">传输记录</span>
              <span class="queue-badge">{{ transfers.length }}</span>
              <span class="spacer"></span>
              <button class="queue-act" title="全部暂停"
                      :disabled="!runningCount && !pendingCount" @click="pauseAllTasks">
                <svg viewBox="0 0 16 16" fill="none" stroke="currentColor"
                     stroke-width="1.8" stroke-linecap="round">
                  <path d="M6 3.4v9.2"/><path d="M10 3.4v9.2"/>
                </svg>
              </button>
              <button class="queue-act" title="全部继续"
                      :disabled="!pausedCount" @click="resumeAllTasks">
                <svg viewBox="0 0 16 16" fill="currentColor">
                  <path d="M5.6 3.4 12.1 8l-6.5 4.6z"/>
                </svg>
              </button>
              <span class="queue-sep"></span>
              <button class="queue-clear" title="清空已完成"
                      :disabled="!successCount" @click="clearDoneTasks">
                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
                     stroke-linecap="round" stroke-linejoin="round">
                  <path d="M4 7h16M9 7V4h6v3M6.5 7l.8 13h9.4l.8-13M10 11v5M14 11v5"/>
                </svg>
              </button>
              <button class="queue-close" title="关闭 (Esc)" @click="queueOpen = false">
                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
                     stroke-linecap="round" stroke-linejoin="round">
                  <path d="M18 6 6 18M6 6l12 12"/>
                </svg>
              </button>
            </div>
            <!-- 筛选标签 -->
            <div class="queue-tabs">
              <button v-for="f in ([
                { k: 'all', label: '全部' },
                { k: 'up', label: '上传' },
                { k: 'down', label: '下载' },
                { k: 'done', label: '已完成' },
              ] as const)" :key="f.k" class="q-tab"
                      :class="{ active: queueFilter === f.k }"
                      @click="queueFilter = f.k">{{ f.label }}</button>
            </div>
            <!-- 任务列表 -->
            <div class="queue-list">
              <div v-if="!transfers.length" class="queue-empty">暂无传输任务</div>
              <div v-for="t in filteredTransfers" :key="t.id"
                   class="sftp-task"
                   :class="[t.dir === 'up' ? 'upload' : 'download', t.status]">
                <div class="task-row">
                  <span class="task-twist-space"></span>
                  <!-- 方向图标 -->
                  <span class="task-ico">
                    <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="2"
                         stroke-linecap="round" stroke-linejoin="round">
                      <path v-if="t.dir==='up'" d="M8 12.6V3.4M3.9 7.5L8 3.4l4.1 4.1"/>
                      <path v-else d="M8 3.4v9.2M3.9 8.5L8 12.6l4.1-4.1"/>
                    </svg>
                  </span>
                  <!-- 主体：名称 / 进度条 / 字节·速率·备注 -->
                  <span class="task-body">
                    <span class="task-name">{{ taskName(t) }}</span>
                    <span class="track"><i class="fill" :style="{ width: Math.min(100, t.pct) + '%' }"></i></span>
                    <span class="task-sub">
                      <span class="bytes">{{ bytesText(t) }}</span>
                      <span v-if="rateText(t)" class="rate">{{ rateText(t) }}</span>
                      <span v-if="noteText(t)" class="note">{{ noteText(t) }}</span>
                    </span>
                  </span>
                  <!-- 状态 + 操作（图标保持不变，按状态显隐） -->
                  <span class="acts">
                    <span class="task-state">{{ stateText(t) }}</span>
                    <!-- 暂停：等待 / 传输中 -->
                    <button v-if="t.status === 'pending' || t.status === 'running'"
                            class="task-abtn" title="暂停" @click="pauseTask(t)">
                      <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.8"
                           stroke-linecap="round">
                        <path d="M6 3.4v9.2"/><path d="M10 3.4v9.2"/>
                      </svg>
                    </button>
                    <!-- 继续：已暂停 -->
                    <button v-if="t.status === 'paused'" class="task-abtn" title="继续"
                            @click="resumeTask(t)">
                      <svg viewBox="0 0 16 16" fill="currentColor">
                        <path d="M5.6 3.4 12.1 8l-6.5 4.6z"/>
                      </svg>
                    </button>
                    <!-- 重试：失败 -->
                    <button v-if="t.status === 'error'" class="task-abtn" title="重试"
                            @click="retryTask(t)">
                      <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.6"
                           stroke-linecap="round" stroke-linejoin="round">
                        <path d="M13.1 8a5.1 5.1 0 1 1-1.6-3.7"/><path d="M13.1 2.7v3.1H10"/>
                      </svg>
                    </button>
                    <!-- 打开目录：完成（上传→远程 / 下载→本地） -->
                    <button v-if="t.status === 'success'" class="task-abtn" title="打开目录"
                            @click="openTaskDir(t)">
                      <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"
                           stroke-linecap="round" stroke-linejoin="round">
                        <path d="M2.3 4.6a1 1 0 0 1 1-1h2.5l1.3 1.6h5.6a1 1 0 0 1 1 1v5.8a1 1 0 0 1-1 1H3.3a1 1 0 0 1-1-1z"/>
                      </svg>
                    </button>
                    <!-- 取消：未完成任务 -->
                    <button v-if="t.status !== 'success'" class="task-x" title="取消传输"
                            @click="removeTask(t)">
                      <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.8"
                           stroke-linecap="round">
                        <path d="M4.3 4.3l7.4 7.4M11.7 4.3l-7.4 7.4"/>
                      </svg>
                    </button>
                    <!-- 清除：已完成记录 -->
                    <button v-else class="task-x" title="清除记录" @click="removeTask(t)">
                      <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"
                           stroke-linecap="round" stroke-linejoin="round">
                        <path d="M3 4.4h10"/><path d="M6.4 4.4V3.2h3.2v1.2"/>
                        <path d="M4.4 4.4l.6 8a1 1 0 0 0 1 .9h4a1 1 0 0 0 1-.9l.6-8"/>
                      </svg>
                    </button>
                  </span>
                </div>
              </div>
            </div>
          </div>

        </div>
      </div>

      <!-- SSH 运行日志 -->
      <div class="dock-pane" :class="{ show: activeTab === 'log' }">
        <div class="log-bar">
          <button
            v-for="f in LEVEL_FILTERS"
            :key="f.key"
            class="log-filter"
            :class="{ active: levelFilter === f.key }"
            @click="levelFilter = f.key"
          >
            {{ f.label }}
            <span class="cnt">{{ f.key === 'ALL' ? logs.length : levelCounts[f.key] }}</span>
          </button>

          <span class="spacer"></span>

          <button class="mini-ico" :class="{ active: searchVisible }" title="搜索日志 (Ctrl/⌘+F)"
                  @click="openSearch">
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
                 stroke-linecap="round" stroke-linejoin="round">
              <circle cx="11" cy="11" r="7"></circle>
              <line x1="21" y1="21" x2="16.65" y2="16.65"></line>
            </svg>
          </button>
          <button class="mini-ico" :class="{ active: autoScroll }"
                  :title="autoScroll ? '自动跟随最新日志（点击暂停）' : '已暂停跟随（点击回到底部）'"
                  @click="toggleAutoScroll">
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
                 stroke-linecap="round" stroke-linejoin="round">
              <polyline points="7 13 12 18 17 13"></polyline>
              <polyline points="7 6 12 11 17 6"></polyline>
            </svg>
          </button>
          <button class="mini-ico" :class="{ active: logWrap }"
                  :title="logWrap ? '自动换行（点击关闭，长行横向滚动）' : '自动换行已关闭（点击开启）'"
                  @click="logWrap = !logWrap">
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
                 stroke-linecap="round" stroke-linejoin="round">
              <line x1="3" y1="7" x2="21" y2="7"></line>
              <path d="M3 12h12.5a2.5 2.5 0 0 1 0 5H11"></path>
              <polyline points="8.5 14 6 17 8.5 20"></polyline>
              <line x1="3" y1="21" x2="14" y2="21"></line>
            </svg>
          </button>
          <button class="mini-ico" title="打开日志目录" @click="revealLogDir">
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
                 stroke-linecap="round" stroke-linejoin="round">
              <path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"></path>
            </svg>
          </button>
          <button class="mini-ico" title="清空日志（仅清内存缓冲，磁盘文件保留）" @click="clearLogs">
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
                 stroke-linecap="round" stroke-linejoin="round">
              <polyline points="3 6 5 6 21 6"></polyline>
              <path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"></path>
              <line x1="10" y1="11" x2="10" y2="17"></line>
              <line x1="14" y1="11" x2="14" y2="17"></line>
            </svg>
          </button>
        </div>

        <div v-show="searchVisible" class="log-search">
          <svg class="ls-ico" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
               stroke-linecap="round" stroke-linejoin="round">
            <circle cx="11" cy="11" r="7"></circle>
            <line x1="21" y1="21" x2="16.65" y2="16.65"></line>
          </svg>
          <input
            ref="searchInputEl"
            v-model="keyword"
            class="ls-input"
            :class="{ nomatch: searchActive && !matchIds.length }"
            type="text"
            placeholder="搜索日志内容…"
            @keydown.enter.exact.prevent="gotoMatch(1)"
            @keydown.shift.enter.prevent="gotoMatch(-1)"
            @keydown.esc.prevent="closeSearch"
          >
          <span class="ls-count" :class="{ zero: searchActive && !matchIds.length }">
            <template v-if="searchActive">
              {{ matchIds.length ? currentMatchIdx + 1 : 0 }} / {{ matchIds.length }}
            </template>
          </span>
          <button class="ls-btn" :class="{ on: caseSensitive }" title="区分大小写"
                  @click="caseSensitive = !caseSensitive">Aa</button>
          <button class="ls-btn" title="上一个匹配 (Shift+Enter)" @click="gotoMatch(-1)">
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2"
                 stroke-linecap="round" stroke-linejoin="round">
              <polyline points="18 15 12 9 6 15"></polyline>
            </svg>
          </button>
          <button class="ls-btn" title="下一个匹配 (Enter)" @click="gotoMatch(1)">
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2"
                 stroke-linecap="round" stroke-linejoin="round">
              <polyline points="6 9 12 15 18 9"></polyline>
            </svg>
          </button>
          <button class="ls-btn" title="关闭 (Esc)" @click="closeSearch">
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2"
                 stroke-linecap="round" stroke-linejoin="round">
              <line x1="18" y1="6" x2="6" y2="18"></line>
              <line x1="6" y1="6" x2="18" y2="18"></line>
            </svg>
          </button>
        </div>

        <div ref="logListEl" class="log-list" :class="{ nowrap: !logWrap }" @scroll="onLogScroll">
          <div v-if="!savedSettings.logCollect" class="log-empty">日志采集已关闭（设置 → 日志 → 日志采集）</div>
          <div v-else-if="!filteredLogs.length" class="log-empty">暂无日志</div>
          <template v-else>
          <div
            v-for="l in filteredLogs"
            :key="l.id"
            class="log-row"
            :class="[l.level, {
              'match-current': searchActive && l.id === currentMatchId,
            }]"
          >
            <span class="log-time">{{ l.time }}</span>
            <span class="log-level">{{ l.level }}</span>
            <span class="log-msg">
              <template v-for="(seg, i) in highlightParts(l.msg)" :key="i">
                <mark v-if="seg.hit" class="log-hit"
                      :class="{ current: l.id === currentMatchId }">{{ seg.text }}</mark>
                <template v-else>{{ seg.text }}</template>
              </template>
            </span>
          </div>
          </template>
        </div>
      </div>

      <!-- 端口转发（规则启停与运行态观测；规则编辑在主机配置面板） -->
      <div class="dock-pane" :class="{ show: activeTab === 'tunnel' }">
        <TunnelPane />
      </div>
    </div>

    <!-- 右键菜单（Teleport 到 body，定位与键盘导航由组件内部完成） -->
    <ContextMenu v-model:visible="menuVisible" :x="menuX" :y="menuY" :items="menuItems" />

    <!-- 修改文件权限（远端栏右键 → 修改权限） -->
    <PermModal v-model:visible="permVisible" :entry="permModalEntry" :busy="permBusy" @apply="applyPerm" />
  </section>
</template>
