<script setup lang="ts">
import { computed, onUnmounted, ref, watch } from 'vue'
import { toast } from '../composables/useToast'
import { COLOR_MAP } from '../data/mockHosts'
import {
  FALLBACK_GROUP,
  groupStats,
  openGroupModal,
  removeGroup,
  type GroupStat,
} from '../stores/groups'
import { filter, groupFilter } from '../stores/hosts'
import { groupedView } from '../stores/hostDisplay'
import { openSession } from '../stores/session'
import type { Host, HostStatus } from '../types'

/* ================= 关键词筛选（与平铺视图共用工具栏搜索框） ================= */
const q = computed(() => filter.value.trim().toLowerCase())

function matches(h: Host, k: string) {
  return (
    h.id.toLowerCase().includes(k) ||
    h.ip.includes(k) ||
    h.user.toLowerCase().includes(k) ||
    h.os.toLowerCase().includes(k) ||
    h.tag.toLowerCase().includes(k)
  )
}

/** 分组名命中时展示组内全部主机，否则只展示命中主机；无命中的分组隐藏 */
const visible = computed<GroupStat[]>(() => {
  const k = q.value
  const out: GroupStat[] = []
  for (const s of groupStats.value) {
    const nameHit = !k || s.def.name.toLowerCase().includes(k)
    const list = nameHit ? s.hosts : s.hosts.filter(h => matches(h, k))
    if (k && !nameHit && !list.length) continue
    out.push({
      def: s.def,
      hosts: list,
      total: list.length,
      online: list.filter(h => h.status === 'online').length,
      warn: list.filter(h => h.status === 'warn').length,
      offline: list.filter(h => h.status === 'offline').length,
    })
  }
  return out
})

/* ================= 展开收拢分组下的主机 ================= */
const expanded = ref<Record<string, boolean>>({})

function isOpen(s: GroupStat) {
  return !!expanded.value[s.def.name]
}

function toggle(s: GroupStat) {
  if (!s.total) return
  expanded.value[s.def.name] = !expanded.value[s.def.name]
}

/** 搜索命中时自动展开，避免命中的主机藏在收起的分组里 */
watch(q, k => {
  if (!k) return
  for (const s of visible.value) expanded.value[s.def.name] = true
})

function openHost(h: Host) {
  openSession(h.id)
}

const STATUS_LABEL: Record<HostStatus, string> = {
  online: '在线',
  idle: '空闲',
  warn: '告警',
  offline: '离线',
}

/* ================= 只看某分组（切回平铺 + 分组筛选） ================= */
function openGroup(s: GroupStat) {
  filter.value = ''
  groupFilter.value = s.def.name
  groupedView.value = false
}

/* ================= 编辑 ================= */
function startRename(e: MouseEvent, s: GroupStat) {
  e.stopPropagation()
  clearConfirm()
  openGroupModal(s.def)
}

/* ================= 删除：两步内联确认（与主机列表一致） ================= */
const confirmingName = ref<string | null>(null)
let confirmTimer: ReturnType<typeof setTimeout> | null = null

function clearConfirm() {
  confirmingName.value = null
  if (confirmTimer) {
    clearTimeout(confirmTimer)
    confirmTimer = null
  }
}

function onDeleteClick(e: MouseEvent, s: GroupStat) {
  e.stopPropagation()
  if (s.def.name === FALLBACK_GROUP) return
  if (confirmingName.value === s.def.name) {
    clearConfirm()
    const moved = removeGroup(s.def.name)
    toast(
      moved
        ? `已删除分组「${s.def.name}」，${moved} 台主机移入「${FALLBACK_GROUP}」`
        : `已删除分组「${s.def.name}」`,
      'ok',
      2400,
    )
    return
  }
  confirmingName.value = s.def.name
  if (confirmTimer) clearTimeout(confirmTimer)
  confirmTimer = setTimeout(() => {
    confirmingName.value = null
    confirmTimer = null
  }, 2600)
}

onUnmounted(clearConfirm)

function accent(s: GroupStat) {
  return { '--accent': COLOR_MAP[s.def.color] }
}
</script>

<template>
  <div class="gp-list">
    <div v-for="s in visible" :key="s.def.name" class="gp-item">
      <div class="gp-row" :style="accent(s)" @click="toggle(s)">
        <button
          type="button"
          class="gp-caret"
          :class="{ open: isOpen(s), empty: !s.total }"
          :disabled="!s.total"
          :title="!s.total ? '该分组暂无主机' : isOpen(s) ? '收拢主机' : '展开主机'"
          @click.stop="toggle(s)"
        >
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4"
               stroke-linecap="round" stroke-linejoin="round">
            <polyline points="9 6 15 12 9 18" />
          </svg>
        </button>
        <div class="gp-icon" :class="'icon-' + s.def.color">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
               stroke-linecap="round" stroke-linejoin="round">
            <path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z" />
          </svg>
        </div>

        <div class="gp-main">
          <div class="gp-name">{{ s.def.name }}</div>
          <div class="gp-count">
            {{ s.total }} 台主机
            <template v-if="s.total">
              · 在线 <span class="gp-state ok">{{ s.online }}</span>
              · 告警 <span class="gp-state warn">{{ s.warn }}</span>
              · 离线 <span class="gp-state off">{{ s.offline }}</span>
            </template>
          </div>
        </div>

        <span class="proj-actions gp-actions" @mouseleave="clearConfirm">
          <button type="button" class="proj-act" title="只看该分组" @click.stop="openGroup(s)">
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4"
                 stroke-linecap="round" stroke-linejoin="round">
              <path d="M5 12h14" />
              <path d="m12 5 7 7-7 7" />
            </svg>
          </button>
          <button type="button" class="proj-act" title="编辑分组" @click="startRename($event, s)">
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
                 stroke-linecap="round" stroke-linejoin="round">
              <path d="M17 3a2.83 2.83 0 1 1 4 4L7.5 20.5 2 22l1.5-5.5Z"></path>
            </svg>
          </button>
          <button
            type="button"
            class="proj-act del"
            :class="{ armed: confirmingName === s.def.name }"
            :title="
              s.def.name === FALLBACK_GROUP
                ? '兜底分组不可删除'
                : confirmingName === s.def.name
                  ? '再次点击确认删除'
                  : '删除分组（主机移入「' + FALLBACK_GROUP + '」）'
            "
            :disabled="s.def.name === FALLBACK_GROUP"
            @click="onDeleteClick($event, s)"
          >
            <svg v-if="confirmingName !== s.def.name" viewBox="0 0 24 24" fill="none"
                 stroke="currentColor" stroke-width="2" stroke-linecap="round"
                 stroke-linejoin="round">
              <polyline points="3 6 5 6 21 6"></polyline>
              <path d="M19 6l-1 14a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2L5 6"></path>
              <path d="M10 11v6M14 11v6"></path>
              <path d="M9 6V4a1 1 0 0 1 1-1h4a1 1 0 0 1 1 1v2"></path>
            </svg>
            <span v-else class="del-confirm">确认删除</span>
          </button>
        </span>
      </div>

      <div v-if="isOpen(s)" class="gp-hosts">
        <div
          v-for="h in s.hosts"
          :key="h.id"
          class="gp-host"
          :title="'连接 ' + h.id"
          @click.stop="openHost(h)"
        >
          <span class="gp-host-icon" :class="'icon-' + h.color">{{ h.label }}</span>
          <span class="gp-host-main">
            <span class="gp-host-name">{{ h.id }}</span>
            <span class="gp-host-meta">{{ h.user }}@{{ h.ip }}:{{ h.port }} · {{ h.os }}</span>
          </span>
          <span class="gp-host-state" :class="h.status">{{ STATUS_LABEL[h.status] }}</span>
        </div>
      </div>
    </div>

    <!-- 新建分组：列表末尾的虚线行 -->
    <button type="button" class="gp-new" @click="openGroupModal()">
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4"
           stroke-linecap="round">
        <path d="M12 5v14M5 12h14" />
      </svg>
      新建分组
    </button>

    <!-- 空状态 -->
    <div v-if="!visible.length" class="empty-state">
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8"
           stroke-linecap="round" stroke-linejoin="round">
        <path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z" />
      </svg>
      <p>没有找到匹配的分组或主机</p>
    </div>
  </div>
</template>
