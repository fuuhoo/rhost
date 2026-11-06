<script setup lang="ts">
import MonitorPanel from '../components/wb/MonitorPanel.vue'
import WbTabs from '../components/wb/WbTabs.vue'
import TerminalPane from '../components/wb/TerminalPane.vue'
import DockPanel from '../components/wb/DockPanel.vue'
import QuickConnectModal from '../components/wb/QuickConnectModal.vue'
import { dockVisible, monitorVisible, sftpFillActive } from '../stores/session'
</script>

<template>
  <div class="workbench-root">
    <div class="body">
      <MonitorPanel v-show="monitorVisible" />

      <main class="workspace" :class="{ 'sftp-fill': sftpFillActive }">
        <WbTabs />

        <!-- sftp:// 外部连接无 PTY：终端让位，由底部面板（SFTP）占满工作区 -->
        <TerminalPane v-show="!sftpFillActive" />

        <DockPanel v-show="dockVisible || sftpFillActive" />
      </main>
    </div>

    <!-- 标签栏「+」的落点：自身 Teleport 到 body，故可与工作台同生命周期常驻 -->
    <QuickConnectModal />
  </div>
</template>
