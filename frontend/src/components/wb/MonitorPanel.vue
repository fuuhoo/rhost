<script setup lang="ts">
import { onMounted, onUnmounted, ref } from 'vue'
import MonitorPane from './MonitorPane.vue'
import { MONITOR_MAX_W, MONITOR_MIN_W, monitorVisible, monitorWidth } from '../../stores/session'

const railRef = ref<HTMLElement | null>(null)
const resizing = ref(false)

/** 拖拽时需要给终端保留的最小宽度（与 RightPanel 的 TERM_MIN_W 同口径：默认 14px 字号下约 80 列） */
const TERM_MIN_W = 700

/** 当前窗口下允许的面板宽度上限：既受设置项区间，也受终端最小宽度约束 */
function maxAllowed() {
  return Math.max(MONITOR_MIN_W, Math.min(MONITOR_MAX_W, window.innerWidth - TERM_MIN_W - 44))
}

let dragStart: { x: number; width: number } | null = null

function onMove(ev: MouseEvent) {
  if (!dragStart) return
  // 面板在左侧：向右拖加宽，向左拖变窄
  monitorWidth.value = Math.min(maxAllowed(), Math.max(MONITOR_MIN_W, dragStart.width + (ev.clientX - dragStart.x)))
}

function onUp() {
  dragStart = null
  resizing.value = false
  document.removeEventListener('mousemove', onMove)
  document.removeEventListener('mouseup', onUp)
}

function onMouseDown(e: MouseEvent) {
  if (!railRef.value) return
  dragStart = { x: e.clientX, width: monitorWidth.value }
  resizing.value = true
  document.addEventListener('mousemove', onMove)
  document.addEventListener('mouseup', onUp)
}

onMounted(() => {
  // 视口级 clamp：持久化宽度在小窗口下可能挤压终端，挂载时收敛
  const max = maxAllowed()
  if (monitorWidth.value > max) monitorWidth.value = max
})
// 拖拽中途卸载（切回首页）时兜底摘掉全局监听
onUnmounted(onUp)
</script>

<template>
  <aside
    v-show="monitorVisible"
    class="monitor-panel"
    :style="{ width: monitorWidth + 'px' }"
  >
    <!-- 拖拽把手：贴面板右缘的热区 -->
    <div
      ref="railRef"
      class="resize-handle"
      :class="{ resizing }"
      title="拖拽调宽"
      @mousedown.prevent="onMouseDown"
    />

    <div class="panel-head">
      <div class="panel-title"><span class="title-text">监控</span></div>
      <!-- MonitorPane 通过 Teleport 注入的操作按钮（重连 / 自动刷新 / SFTP / 断开） -->
      <div id="mp-head-actions" class="panel-actions" />
      <button class="close-btn" title="折叠监控面板" @click="monitorVisible = false">
        <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor"
             stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <path d="M15 18l-6-6 6-6" />
        </svg>
      </button>
    </div>

    <div class="panel-body">
      <MonitorPane />
    </div>
  </aside>
</template>

<style scoped>
.monitor-panel {
  position: relative;
  flex-shrink: 0;
  background: var(--glass-panel);
  border-right: 1px solid var(--border);
  display: flex;
  flex-direction: column;
  min-width: 0;
  transition: none; /* 拖拽时不加宽度过渡，避免延迟 */
}

/* 拖拽把手：与 RightPanel 同款（隐形热区 + 中央小把手，hover 变绿加长） */
.resize-handle {
  position: absolute;
  right: -4px;
  top: 0;
  bottom: 0;
  width: 9px;
  cursor: col-resize;
  z-index: 10;
}
.resize-handle::after {
  content: '';
  position: absolute;
  left: 50%;
  top: 50%;
  transform: translate(-50%, -50%);
  width: 2px;
  height: 38px;
  border-radius: 2px;
  background: #1f2b39;
  transition: background 0.15s, height 0.15s;
}
.resize-handle:hover::after,
.resize-handle.resizing::after {
  background: var(--green);
  height: 56px;
}

/* 页签头（样式与 RightPanel 头部对齐，保证左右两栏观感一致） */
.panel-head {
  height: 40px;
  padding: 0 8px 0 12px;
  border-bottom: 1px solid var(--border-soft);
  display: flex;
  align-items: center;
  background: var(--panel);
  flex-shrink: 0;
}
.panel-title {
  font-size: 12.5px;
  font-weight: 600;
  color: var(--text);
  margin-right: auto;
  display: flex;
  align-items: center;
  gap: 6px;
  min-width: 0;
}
.panel-actions {
  display: flex;
  align-items: center;
  gap: 4px;
  flex-shrink: 0;
}
.panel-actions:empty {
  display: none;
}
.close-btn {
  width: 26px;
  height: 26px;
  border-radius: 6px;
  display: grid;
  place-items: center;
  background: transparent;
  border: none;
  color: var(--muted-2);
  cursor: pointer;
  transition: background 0.15s, color 0.15s;
  flex-shrink: 0;
  margin-left: 4px;
}
.close-btn:hover {
  background: var(--panel-2);
  color: var(--text);
}

.panel-body {
  flex: 1;
  overflow: hidden;
  display: flex;
  flex-direction: column;
}

/* 窄屏收起左栏（与 RightPanel 同断点，避免两栏同时挤占终端） */
@media (max-width: 1180px) {
  .monitor-panel {
    display: none;
  }
}
</style>
