<script setup lang="ts">
import { appWindow, detectPlatform } from '../lib/tauri'
import { appView, sessions } from '../stores/session'

// macOS 由原生 Overlay 红绿灯渲染（左侧安全区）；Windows/Linux 前端自绘右侧窗口按钮
const showWinCtrl = detectPlatform() !== 'macos'

/** 首页常驻出口：不必从右侧「历史会话」里挑一条才能回工作台 */
function goWorkbench() {
  appView.value = 'workbench'
}

async function onClose() {
  ;(await appWindow())?.close()
}
async function onMinimize() {
  ;(await appWindow())?.minimize()
}
async function onMaximize() {
  const w = await appWindow()
  await w?.toggleMaximize()
}
</script>

<template>
  <header class="titlebar" data-tauri-drag-region>
    <!-- 首页常驻出口放最左：紧挨红绿灯，视线落点就在入口上，不必扫到右端 -->
    <button
      class="tb-btn"
      :title="sessions.length ? `回到工作台（${sessions.length} 个标签）` : '回到工作台'"
      @click="goWorkbench"
    >
      <svg viewBox="0 0 24 24">
        <rect x="2.5" y="4" width="19" height="14" rx="2" />
        <path d="M6 9l3 3-3 3M11 15h6" />
      </svg>
      工作台
    </button>
    <h1 data-tauri-drag-region>欢迎访问 <em>Rhost</em></h1>
    <!-- Windows/Linux：前端自绘窗口控制按钮（macOS 由原生 Overlay 红绿灯渲染） -->
    <div v-if="showWinCtrl" class="winctl">
      <button class="wc-btn" title="最小化" @click="onMinimize">
        <svg viewBox="0 0 10 10"><path d="M0 5h10"/></svg>
      </button>
      <button class="wc-btn" title="最大化" @click="onMaximize">
        <svg viewBox="0 0 10 10"><rect x=".5" y=".5" width="9" height="9" fill="none"/></svg>
      </button>
      <button class="wc-btn wc-close" title="关闭" @click="onClose">
        <svg viewBox="0 0 10 10"><path d="M0 0l10 10M10 0L0 10"/></svg>
      </button>
    </div>
  </header>
</template>
