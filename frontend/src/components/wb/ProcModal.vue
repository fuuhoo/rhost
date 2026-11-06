<script setup lang="ts">
import { computed, onUnmounted, ref, watch } from 'vue'
import { toast } from '../../composables/useToast'
import {
  activeHost, activeMetrics, activeSession, metricsProcsFull,
  type ProcMetricsData,
} from '../../stores/session'
import { copyText } from '../../stores/keys'

/**
 * 进程弹窗：全量进程表，点表头按 CPU / 内存 / 容器 / 进程名排序。
 * 全量列表千级进程约 130KB/帧，只在弹窗打开期间向后端订阅（metrics_procs_full），
 * 关闭立即退订，侧栏 Top 5 榜单不受影响。
 */
const props = defineProps<{ visible: boolean }>()
const emit = defineEmits<{ (e: 'update:visible', v: boolean): void }>()

type SortKey = 'cpu' | 'rss' | 'container' | 'name'
const sortKey = ref<SortKey>('cpu')
/** 数值列默认降序，文本列默认升序；再点同一列表头翻转 */
const sortDir = ref<-1 | 1>(-1)

const COLS: { key: SortKey; label: string; numeric: boolean }[] = [
  { key: 'name', label: '进程', numeric: false },
  { key: 'cpu', label: 'CPU %', numeric: true },
  { key: 'rss', label: '内存', numeric: true },
  { key: 'container', label: '容器', numeric: false },
]

const procs = computed(() => activeMetrics.value?.procs ?? null)
/** 全量未到达前先用 Top 5 垫底，避免打开后 2s 空窗 */
const fullArrived = computed(() => procs.value?.all !== undefined)
const rows = computed<ProcMetricsData[]>(() => procs.value?.all ?? procs.value?.top ?? [])

const sorted = computed(() => {
  const key = sortKey.value
  const dir = sortDir.value
  return [...rows.value].sort((a, b) => {
    let c = 0
    if (key === 'cpu') c = a.cpu - b.cpu
    else if (key === 'rss') c = a.rss - b.rss
    else if (key === 'name') c = a.name.localeCompare(b.name) || a.pid - b.pid
    else {
      // 无容器归属的一律沉底：排序是为了把同容器的进程聚在一起，不是把空行顶到最前
      const ac = a.container.replace(/^\//, '')
      const bc = b.container.replace(/^\//, '')
      if (!ac && !bc) c = a.cpu - b.cpu
      else if (!ac) return 1
      else if (!bc) return -1
      else c = ac.localeCompare(bc) || b.cpu - a.cpu
    }
    return c * dir
  })
})

function toggleSort(key: SortKey) {
  if (sortKey.value === key) {
    sortDir.value = sortDir.value === -1 ? 1 : -1
  } else {
    sortKey.value = key
    sortDir.value = key === 'name' || key === 'container' ? 1 : -1
  }
}
function arrow(key: SortKey) {
  return sortKey.value === key ? (sortDir.value === -1 ? '↓' : '↑') : ''
}

function fmtBytes(n: number) {
  if (!n) return '0 B'
  if (n >= 1024 ** 3) return `${(n / 1024 ** 3).toFixed(1)} GB`
  if (n >= 1024 ** 2) return `${(n / 1024 ** 2).toFixed(0)} MB`
  if (n >= 1024) return `${(n / 1024).toFixed(0)} KB`
  return `${n} B`
}

async function copyProc(text: string, label: string) {
  const ok = await copyText(text)
  toast(ok ? `${label}已复制` : '复制失败', ok ? 'ok' : 'err', 1200)
}

/* 订阅随弹窗开合；会话切换时先退订旧会话再订新的，避免旧会话一直推全量 */
function subscribe(on: boolean, id = activeSession.value?.backendId) {
  if (id) metricsProcsFull(id, on)
}
watch(() => props.visible, v => subscribe(v))
watch(() => activeSession.value?.backendId, (id, old) => {
  if (!props.visible) return
  if (old && old !== id) metricsProcsFull(old, false)
  subscribe(true, id)
})
onUnmounted(() => subscribe(false))

function close() {
  emit('update:visible', false)
}
function onKey(e: KeyboardEvent) {
  if (props.visible && e.key === 'Escape') {
    e.preventDefault()
    close()
  }
}
document.addEventListener('keydown', onKey)
onUnmounted(() => document.removeEventListener('keydown', onKey))
</script>

<template>
  <!-- 弹窗挂到 body：左栏祖先一旦有 transform/contain，fixed 遮罩会被困在栏内 -->
  <Teleport to="body">
  <div class="mask" :class="{ show: visible }" @click.self="close">
    <div class="modal prc-modal" role="dialog" aria-modal="true" aria-labelledby="prcTitle">
      <div class="modal-head">
        <div class="modal-title">
          <h2 id="prcTitle">进程</h2>
          <p>{{ activeHost?.label || activeHost?.ip || '当前会话' }} · 共 {{ procs?.total ?? 0 }} 个进程</p>
        </div>
        <button class="modal-close" title="关闭 (Esc)" @click="close">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round">
            <line x1="18" y1="6" x2="6" y2="18" />
            <line x1="6" y1="6" x2="18" y2="18" />
          </svg>
        </button>
      </div>

      <div class="prc-head">
        <button
          v-for="c in COLS" :key="c.key" class="prc-th" :class="{ num: c.numeric, on: sortKey === c.key }"
          :title="`按${c.label}排序`" @click="toggleSort(c.key)"
        >
          {{ c.label }}<i class="prc-arrow">{{ arrow(c.key) }}</i>
        </button>
      </div>

      <div class="prc-body">
        <div v-if="!rows.length" class="prc-empty">等待采集…</div>
        <div v-for="p in sorted" :key="p.pid" class="prc-row">
          <span class="prc-name" :title="`${p.name} #${p.pid}`">
            {{ p.name }}<button
              type="button" class="prc-pid" :title="`点击复制进程 ID：${p.pid}`"
              @click.stop="copyProc(String(p.pid), '进程 ID')"
            >#{{ p.pid }}</button>
          </span>
          <span class="prc-num" :class="{ hi: p.cpu >= 10, crit: p.cpu >= 20 }">{{ p.cpu.toFixed(1) }}</span>
          <span class="prc-num mem">{{ fmtBytes(p.rss) }}</span>
          <button
            v-if="p.container" type="button" class="prc-cname"
            :title="`点击复制容器名称：${p.container.replace(/^\//, '')}`"
            @click.stop="copyProc(p.container.replace(/^\//, ''), '容器名称')"
          >
            {{ p.container.replace(/^\//, '') }}
          </button>
          <span v-else class="prc-cname none" title="非容器进程">—</span>
        </div>
      </div>

      <div class="prc-foot">
        <span v-if="!fullArrived" class="prc-loading">全量进程加载中，当前先显示 CPU Top {{ rows.length }}…</span>
        <span v-else>显示 {{ sorted.length }} / {{ procs?.total ?? 0 }} 个进程</span>
        <span class="prc-tip">点表头切换排序；再次点击反向；点击 #PID 或容器名可复制</span>
      </div>
    </div>
  </div>
  </Teleport>
</template>

<style scoped>
.prc-modal {
  width: 720px;
  height: min(640px, calc(100vh - 80px));
  display: flex;
  flex-direction: column;
}
.prc-head,
.prc-row {
  display: grid;
  grid-template-columns: minmax(0, 1fr) 86px 100px 170px;
  align-items: center;
  gap: 10px;
  padding: 0 18px;
}
.prc-head {
  flex-shrink: 0;
  border-bottom: 1px solid var(--border);
  background: var(--panel-2);
}
.prc-th {
  display: flex;
  align-items: center;
  gap: 4px;
  padding: 8px 0;
  border: 0;
  background: none;
  font: inherit;
  font-size: 11.5px;
  font-weight: 600;
  color: var(--muted);
  cursor: pointer;
  text-align: left;
}
.prc-th.num { justify-content: flex-end; }
.prc-th:hover { color: var(--text); }
.prc-th.on { color: var(--green); }
.prc-arrow { font-style: normal; width: 10px; }
.prc-body {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  padding: 4px 0;
}
.prc-row {
  padding-top: 5px;
  padding-bottom: 5px;
  font-size: 12px;
  /* 千行表只渲染视口内的行，滚动不卡 */
  content-visibility: auto;
  contain-intrinsic-size: auto 26px;
}
.prc-row:nth-child(even) { background: rgba(128, 128, 128, .06); }
.prc-row:hover { background: var(--panel-2); }
.prc-name {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  color: var(--text);
}
.prc-pid {
  margin-left: 6px;
  padding: 0 3px;
  border: 0;
  border-radius: 3px;
  background: transparent;
  font: inherit;
  font-size: 10.5px;
  line-height: inherit;
  color: var(--muted-2);
  cursor: copy;
}
.prc-pid:hover {
  background: var(--panel-2);
  color: var(--text);
  text-decoration: underline dotted;
}
.prc-cname {
  min-width: 0;
  padding: 0;
  border: 0;
  background: none;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font: inherit;
  font-size: 11px;
  text-align: left;
  color: var(--blue);
  cursor: copy;
}
.prc-cname:hover { text-decoration: underline dotted; }
.prc-pid:focus-visible,
.prc-cname:focus-visible {
  outline: 1px solid var(--blue);
  outline-offset: 1px;
}
.prc-cname.none {
  color: var(--muted-2);
  cursor: default;
}
.prc-num {
  text-align: right;
  font-family: var(--mono, monospace);
  font-size: 11.5px;
  color: var(--text);
}
.prc-num.hi { color: var(--amber, #d19a06); }
.prc-num.crit { color: var(--red, #e5534b); }
.prc-num.mem { color: var(--muted); }
.prc-empty {
  padding: 30px 0;
  text-align: center;
  font-size: 12px;
  color: var(--muted-2);
}
.prc-foot {
  flex-shrink: 0;
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  padding: 9px 18px;
  border-top: 1px solid var(--border);
  font-size: 11px;
  color: var(--muted-2);
}
.prc-loading { color: var(--amber, #d19a06); }
</style>
