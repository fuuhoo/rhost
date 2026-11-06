<script setup lang="ts">
import { computed, nextTick, ref, watch } from 'vue'
import { hosts } from '../../stores/hosts'
import type { Host } from '../../types'
import {
  clearRecentConnections, openSession, recentConnections, showQuickConnect,
} from '../../stores/session'

/**
 * 快速连接弹窗：标签栏「+」的落点。
 * 做成浮层而不是页签，是为了不顶掉当前终端画面——选完即关，回到原来那个会话。
 */
const query = ref('')
const cursor = ref(0)
const inputEl = ref<HTMLInputElement | null>(null)
const listEl = ref<HTMLDivElement | null>(null)

/** 最近连接优先，其余按名称；与空白标签里的快速连接列表同一套排序口径 */
const ordered = computed<Host[]>(() => {
  const seen = new Set<string>()
  const rows: Host[] = []
  for (const r of recentConnections.value) {
    const h = hosts.value.find(x => x.id === r.hostId)
    if (h && !seen.has(h.id)) {
      seen.add(h.id)
      rows.push(h)
    }
  }
  const rest = hosts.value
    .filter(h => !seen.has(h.id))
    .sort((a, b) => a.id.localeCompare(b.id, 'zh-CN'))
  return [...rows, ...rest]
})

const recentCount = computed(() => {
  const ids = new Set(hosts.value.map(h => h.id))
  return recentConnections.value.filter(r => ids.has(r.hostId)).length
})

const rows = computed(() => {
  const q = query.value.trim().toLowerCase()
  if (!q) return ordered.value
  return ordered.value.filter(h =>
    [h.id, h.user, `${h.user}@${h.ip}`, `${h.ip}:${h.port}`, h.group, h.tag]
      .some(s => (s || '').toLowerCase().includes(q)),
  )
})

function dotClass(h: Host) {
  return h.status === 'online' ? 'online' : h.status === 'offline' ? 'offline' : 'warn'
}

function close() {
  showQuickConnect.value = false
}

function pick(h: Host) {
  if (h.status === 'offline') return
  close()
  openSession(h.id)
}

/** Enter 连接高亮行；↑↓ 移动高亮并把该行滚进视野 */
function onKey(e: KeyboardEvent) {
  if (e.key === 'Escape') {
    e.preventDefault()
    close()
    return
  }
  if (!rows.value.length) return
  if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
    e.preventDefault()
    const step = e.key === 'ArrowDown' ? 1 : -1
    cursor.value = (cursor.value + step + rows.value.length) % rows.value.length
    listEl.value?.children[cursor.value]?.scrollIntoView({ block: 'nearest' })
    return
  }
  if (e.key === 'Enter') {
    e.preventDefault()
    pick(rows.value[cursor.value]!)
  }
}

watch(showQuickConnect, v => {
  if (!v) return
  query.value = ''
  cursor.value = 0
  void nextTick(() => inputEl.value?.focus())
})
</script>

<template>
  <!-- 挂到 body：工作台任一祖先带 transform/contain 会把 fixed 遮罩困在栏内 -->
  <Teleport to="body">
    <div v-if="showQuickConnect" class="mask show" @click.self="close">
      <div class="modal qc-modal" role="dialog" aria-modal="true" aria-labelledby="qcTitle">
        <div class="modal-head">
          <div class="modal-title">
            <h2 id="qcTitle">快速连接</h2>
            <p>共 {{ hosts.length }} 台已保存主机 · 最近连接 {{ recentCount }} 台置顶</p>
          </div>
          <button v-if="recentCount > 0" class="qc-clear" title="清空最近连接历史" @click="clearRecentConnections">
            清空最近
          </button>
          <button class="modal-close" title="关闭 (Esc)" @click="close">
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round">
              <line x1="18" y1="6" x2="6" y2="18" />
              <line x1="6" y1="6" x2="18" y2="18" />
            </svg>
          </button>
        </div>

        <div class="qc-search">
          <input
            ref="inputEl"
            v-model="query"
            type="text"
            spellcheck="false"
            autocomplete="off"
            autocapitalize="off"
            placeholder="搜索名称 / 地址 / 用户 / 分组"
            @keydown="onKey"
          >
        </div>

        <div ref="listEl" class="qc-body">
          <button
            v-for="(h, i) in rows" :key="h.id"
            class="qc-row" :class="{ on: i === cursor, off: h.status === 'offline' }"
            @mouseenter="cursor = i"
            @click="pick(h)"
          >
            <span class="dot" :class="dotClass(h)"></span>
            <span class="qc-name">{{ h.id }}</span>
            <span class="qc-addr">{{ h.group || '/' }} · {{ h.user }}@{{ h.ip }}:{{ h.port }}</span>
            <em v-if="i < recentCount && !query" class="qc-tag">最近</em>
          </button>
          <div v-if="!rows.length" class="qc-none">
            {{ hosts.length ? '没有匹配的主机' : '还没有已保存的主机' }}
          </div>
        </div>

        <div class="qc-foot">
          <span>↑↓ 选择 · Enter 连接 · Esc 关闭</span>
          <span class="qc-note">连接失败或钥匙串里没有密码时，会先问一次密码再连</span>
        </div>
      </div>
    </div>
  </Teleport>
</template>

<style scoped>
.qc-modal {
  width: 620px;
  max-width: calc(100vw - 48px);
  display: flex;
  flex-direction: column;
}
.qc-clear {
  flex: none;
  margin-right: 8px;
  padding: 4px 10px;
  border: 1px solid var(--border);
  border-radius: 6px;
  background: var(--panel-2);
  color: var(--text-soft);
  font-size: 12px;
  cursor: pointer;
}
.qc-clear:hover {
  color: var(--text-strong);
  border-color: var(--border-strong);
}
.qc-search {
  flex: none;
  padding: 10px 18px;
  border-bottom: 1px solid var(--border);
}
.qc-search input {
  width: 100%;
  padding: 7px 10px;
  border: 1px solid var(--border);
  border-radius: 7px;
  background: var(--panel);
  color: var(--text-strong);
  font-size: 13px;
  outline: none;
}
.qc-search input:focus {
  border-color: var(--accent);
  background: var(--input-focus-bg);
}
.qc-body {
  flex: 1;
  min-height: 120px;
  max-height: min(420px, calc(100vh - 260px));
  overflow-y: auto;
  padding: 6px;
}
.qc-row {
  display: grid;
  grid-template-columns: 12px minmax(110px, 1fr) minmax(0, 1.6fr) auto;
  align-items: center;
  gap: 10px;
  width: 100%;
  padding: 8px 12px;
  border: 0;
  border-radius: 7px;
  background: none;
  color: var(--text-strong);
  text-align: left;
  font-size: 13px;
  cursor: pointer;
}
.qc-row.on {
  background: var(--hover);
}
.qc-row.off {
  color: var(--muted);
  cursor: not-allowed;
}
.qc-name {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  font-weight: 600;
}
.qc-addr {
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  color: var(--text-soft);
  font-size: 12px;
}
.qc-tag {
  flex: none;
  padding: 1px 7px;
  border-radius: 999px;
  background: rgba(var(--accent-rgb), .14);
  color: var(--accent);
  font-size: 11px;
  font-style: normal;
}
.qc-none {
  padding: 28px 0;
  text-align: center;
  color: var(--muted);
  font-size: 12.5px;
}
.qc-foot {
  flex: none;
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 12px;
  padding: 9px 18px;
  border-top: 1px solid var(--border);
  background: var(--panel-2);
  color: var(--muted);
  font-size: 11.5px;
}
.qc-note {
  color: var(--muted-2);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
</style>
