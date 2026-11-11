<script setup lang="ts">
import { appWindow, detectPlatform } from '../../lib/tauri'
import AppLogo from '../AppLogo.vue'
import { showNewConn } from '../../stores/hosts'
import { showSettings } from '../../stores/settings'
import { showPalette, showToolbox } from '../../lib/uiBus'
import { toast } from '../../composables/useToast'
import {
  activeSession, appView, dockVisible, monitorVisible, openDock, rightPanelVisible, searchTick, toggleDock,
} from '../../stores/session'

// macOS 由原生 Overlay 红绿灯渲染（左侧安全区）；Windows/Linux 前端自绘右侧窗口按钮
const showWinCtrl = detectPlatform() !== 'macos'

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

function onNew() {
  showNewConn.value = true
}

function goHome() {
  appView.value = 'home'
}

function onSearch() {
  searchTick.value++
}

function onSftp() {
  openDock('sftp')
  toast('已打开「文件」面板', 'info', 1600)
}

function onToolbox() {
  showToolbox.value = true
}

function onPaletteClick() {
  showPalette.value = true
}

/** 通知中心尚无独立子系统；应用真实的事件流就是底部面板的运行日志页签 */
function onNotify() {
  openDock('log')
}

function onSettings() {
  showSettings.value = true
}
</script>

<template>
  <header class="titlebar wb" data-tauri-drag-region>
    <!-- 品牌（点击返回首页；不参与窗口拖拽以接收点击）。
         macOS 下左侧安全区由 CSS 预留，避免与原生 Overlay 红绿灯重叠。 -->
    <div class="logo" title="返回首页" @click="goHome">
      <AppLogo />
      <span>Rhost</span>
    </div>

    <!-- 新建 -->
    <button class="tb-btn primary" title="新建连接" @click="onNew">
      <svg viewBox="0 0 24 24"><path d="M12 5v14M5 12h14"/></svg>
      <span class="btn-text">新建</span>
    </button>

    <!-- 搜索 -->
    <button class="tb-btn" title="搜索日志 (⌘F)" @click="onSearch">
      <svg viewBox="0 0 24 24"><circle cx="11" cy="11" r="7"/><path d="M20 20l-3.5-3.5"/></svg>
    </button>

    <!-- SFTP -->
    <button class="tb-btn" title="打开文件面板（SFTP）" @click="onSftp">
      <svg viewBox="0 0 24 24"><path d="M4 7a2 2 0 012-2h3l2 2h7a2 2 0 012 2v8a2 2 0 01-2 2H6a2 2 0 01-2-2V7z"/></svg>
    </button>

    <!-- 工具箱：防火墙 / 端口 / 公钥 / Docker / 系统日志 -->
    <button
      class="tb-btn" :disabled="!activeSession"
      :title="activeSession ? '工具箱：防火墙 / 端口 / 公钥 / Docker / 系统日志' : '连接主机后可用'"
      @click="onToolbox"
    >
      <svg viewBox="0 0 24 24">
        <rect x="3" y="9" width="18" height="11" rx="2"/>
        <path d="M8.5 9V7a2 2 0 012-2h3a2 2 0 012 2v2"/>
        <path d="M3 13h18"/>
        <path d="M9.5 13v2.5h5V13"/>
      </svg>
    </button>

    <div class="tb-spacer"></div>

    <!-- 命令面板入口 -->
    <button class="tb-cmdk" title="执行命令…" @click="onPaletteClick">
      <svg viewBox="0 0 24 24"><path d="M9 6l6 6-6 6"/></svg>
      <span class="text">执行命令…</span>
      <span class="kbd"><kbd>⌘</kbd><kbd>K</kbd></span>
    </button>

    <!-- 视图切换组 -->
    <div class="tb-group">
      <button
        class="tb-toggle"
        :class="{ active: monitorVisible }"
        title="显示 / 隐藏监控面板（左栏）"
        @click="monitorVisible = !monitorVisible"
      >
        <svg viewBox="0 0 24 24"><rect x="3" y="4" width="18" height="16" rx="2"/><path d="M9 4v16"/></svg>
      </button>
      <button
        class="tb-toggle"
        :class="{ active: dockVisible }"
        title="显示 / 隐藏底部面板"
        @click="toggleDock"
      >
        <svg viewBox="0 0 24 24"><rect x="3" y="4" width="18" height="16" rx="2"/><path d="M3 15h18"/></svg>
      </button>
      <button
        class="tb-toggle"
        :class="{ active: rightPanelVisible }"
        title="显示 / 隐藏右侧面板"
        @click="rightPanelVisible = !rightPanelVisible"
      >
        <svg viewBox="0 0 24 24"><rect x="3" y="4" width="18" height="16" rx="2"/><path d="M15 4v16"/></svg>
      </button>
    </div>

    <!-- 通知：打开底部运行日志（应用事件流） -->
    <button class="tb-btn" title="运行日志" @click="onNotify">
      <svg viewBox="0 0 24 24"><path d="M18 8A6 6 0 006 8c0 7-3 9-3 9h18s-3-2-3-9"/><path d="M13.7 21a2 2 0 01-3.4 0"/></svg>
    </button>

    <!-- 设置 -->
    <button class="tb-btn" title="设置" @click="onSettings">
      <svg viewBox="0 0 24 24"><circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.7 1.7 0 00.3 1.9l.1.1a2 2 0 11-2.8 2.8l-.1-.1a1.7 1.7 0 00-1.9-.3 1.7 1.7 0 00-1 1.5V21a2 2 0 11-4 0v-.1a1.7 1.7 0 00-1.1-1.5 1.7 1.7 0 00-1.9.3l-.1.1a2 2 0 11-2.8-2.8l.1-.1a1.7 1.7 0 00.3-1.9 1.7 1.7 0 00-1.5-1H3a2 2 0 110-4h.1a1.7 1.7 0 001.5-1.1 1.7 1.7 0 00-.3-1.9l-.1-.1a2 2 0 112.8-2.8l.1.1a1.7 1.7 0 001.9.3h.1a1.7 1.7 0 001-1.5V3a2 2 0 114 0v.1a1.7 1.7 0 001 1.5 1.7 1.7 0 001.9-.3l.1-.1a2 2 0 112.8 2.8l-.1.1a1.7 1.7 0 00-.3 1.9v.1a1.7 1.7 0 001.5 1H21a2 2 0 110 4h-.1a1.7 1.7 0 00-1.5 1z"/></svg>
    </button>

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
