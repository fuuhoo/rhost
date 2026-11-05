<script setup lang="ts">
import { toast } from '../../composables/useToast'
import { openSession, recentConnections } from '../../stores/session'
import { hosts } from '../../stores/hosts'

/** 点击历史会话：主机已被删除时仅提示，保留记录 */
function onRecentClick(r: { hostId: string }) {
  if (!hosts.value.some(h => h.id === r.hostId)) {
    toast('主机已删除', 'warn')
    return
  }
  openSession(r.hostId)
}
</script>

<template>
  <div class="hp-root">
    <div v-if="recentConnections.length === 0" class="hp-empty">暂无历史会话</div>
    <button
      v-for="r in recentConnections"
      :key="r.hostId"
      class="hp-item"
      :title="`连接到 ${r.name}`"
      @click="onRecentClick(r)"
    >
      <span class="hp-name">{{ r.name }}</span>
      <span class="hp-addr">{{ r.user }}@{{ r.ip }}:{{ r.port }}</span>
    </button>
  </div>
</template>

<style scoped>
/* 首页右栏「历史会话」页签内容：最近成功连接的主机（含已关闭），点击快速重连 */
.hp-root {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  overscroll-behavior: contain;
  padding: 6px 8px 12px;
}
.hp-root::-webkit-scrollbar {
  width: 8px;
}
.hp-root::-webkit-scrollbar-thumb {
  background: var(--scrollbar-thumb);
  border-radius: 4px;
  border: 2px solid transparent;
  background-clip: content-box;
}
.hp-empty {
  font-size: 11.5px;
  color: var(--muted);
  padding: 12px 0;
  text-align: center;
}
.hp-item {
  width: 100%;
  display: flex;
  flex-direction: column;
  align-items: flex-start;
  gap: 2px;
  padding: 8px 10px;
  border-radius: 8px;
  border: 1px solid transparent;
  background: transparent;
  cursor: pointer;
  transition: background 0.15s, border-color 0.15s;
  text-align: left;
  font-family: inherit;
}
.hp-item:hover {
  background: var(--panel-2);
  border-color: var(--border);
}
.hp-name {
  width: 100%;
  font-size: 12px;
  color: var(--text);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.hp-addr {
  width: 100%;
  font-size: 11px;
  color: var(--muted);
  font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
</style>
