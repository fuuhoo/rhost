<script setup lang="ts">
import { computed, watch } from 'vue'
import { appView, rightPanelTab, rightPanelVisible } from '../stores/session'
import { savedSettings } from '../stores/settings'

const isHome = computed(() => appView.value === 'home')
const aiEnabled = computed(() => savedSettings.aiEnabled)

/** AI 关闭时若正停留在 AI 页签，折叠面板并回退到主机页签 */
watch(aiEnabled, on => {
  if (!on && rightPanelTab.value === 'ai') {
    rightPanelTab.value = 'hosts'
    rightPanelVisible.value = false
  }
})

/** 点击图标：面板折叠→展开并切到对应页签；当前页签激活中→折叠；否则仅切换页签 */
function onIcon(tab: 'hosts' | 'ai') {
  if (rightPanelVisible.value && rightPanelTab.value === tab) {
    rightPanelVisible.value = false
  } else {
    rightPanelTab.value = tab
    rightPanelVisible.value = true
  }
}
</script>

<template>
  <aside class="icon-rail">
    <button
      class="rail-btn"
      :class="{ on: rightPanelVisible && rightPanelTab === 'hosts' }"
      :title="isHome ? '历史会话' : '主机列表'"
      @click="onIcon('hosts')"
    >
      <!-- 首页：历史会话（时钟）；工作台：主机列表（服务器机架） -->
      <svg v-if="isHome" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor"
           stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
        <circle cx="12" cy="12" r="9" />
        <polyline points="12 7 12 12 15.5 14" />
      </svg>
      <svg v-else width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor"
           stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
        <rect x="3" y="4" width="18" height="7" rx="2" />
        <rect x="3" y="13" width="18" height="7" rx="2" />
        <path d="M7 7.5h.01M7 16.5h.01" />
      </svg>
    </button>
    <button
      v-if="aiEnabled"
      class="rail-btn"
      :class="{ on: rightPanelVisible && rightPanelTab === 'ai' }"
      title="AI 助手 (⌘I)"
      @click="onIcon('ai')"
    >
      <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor"
           stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
        <path d="M12 3l1.9 5.6L19.5 10l-5.6 1.9L12 17.5l-1.9-5.6L4.5 10l5.6-1.4z" />
        <path d="M19 15l.9 2.6 2.6.9-2.6.9-.9 2.6-.9-2.6-2.6-.9 2.6-.9z" />
      </svg>
    </button>
  </aside>
</template>

<style scoped>
.icon-rail {
  width: 44px;
  flex-shrink: 0;
  background: var(--glass-panel);
  border-left: 1px solid var(--border);
  display: flex;
  flex-direction: column;
  align-items: center;
  padding-top: 10px;
  gap: 6px;
  user-select: none;
}
.rail-btn {
  width: 32px;
  height: 32px;
  border-radius: 8px;
  display: grid;
  place-items: center;
  background: transparent;
  border: 1px solid transparent;
  color: var(--muted-2);
  cursor: pointer;
  transition: background 0.15s, color 0.15s, border-color 0.15s;
}
.rail-btn:hover {
  background: var(--panel-2);
  border-color: var(--border);
  color: var(--text);
}
.rail-btn:active {
  transform: scale(0.94);
}
.rail-btn.on {
  color: var(--green);
  background: rgba(61, 220, 132, 0.1);
  border-color: rgba(61, 220, 132, 0.35);
}

/* 窄屏不再整体隐藏右侧栏：图标栏是浮层面板（主机 / 历史 / AI）的唯一入口，
   面板在 ≤1180px 改为浮层，见 RightPanel 的同断点规则 */
</style>
