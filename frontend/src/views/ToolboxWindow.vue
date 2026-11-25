<script setup lang="ts">
/**
 * 工具箱独立窗口的根组件。
 *
 * Rust 的 open_toolbox_window 建出第二个 Webview，加载同一份 bundle，并把会话信息
 * 首帧注入到 window.__RHOST_TOOLBOX__；main.ts 见到该注入就挂这个组件而不是 App.vue。
 *
 * 会话不复制：注入的信息造一条影子会话（见 adoptDetachedSession），工具箱里的命令
 * 仍按 backendId 打到后端那条 SSH 会话上，因此关掉主窗口的弹窗后采集照旧。
 */
import { onMounted, ref } from 'vue'
import ToolboxModal from '../components/wb/ToolboxModal.vue'
import ConfirmModal from '../components/ConfirmModal.vue'
import PasswordModal from '../components/PasswordModal.vue'
import AppToast from '../components/AppToast.vue'
import { loadAppConfig } from '../stores/appConfig'
import { adoptDetachedSession } from '../stores/session'
import { showToolbox } from '../lib/uiBus'
import { appWindow, toolboxWindowMeta } from '../lib/tauri'
import { onToolboxSessionGone } from '../lib/toolboxWindowSync'
import { toast } from '../composables/useToast'

const ready = ref(false)
const broken = ref('')

onMounted(async () => {
  const meta = toolboxWindowMeta()
  if (!meta) {
    broken.value = '缺少会话信息，请从工具箱弹窗重新打开独立窗口'
    ;(window as any).__hideSplash?.()
    return
  }
  // 先把界面挂起来，再去拉配置：配置 IPC 慢一点、或者干脆失败，都不该让窗口只剩一块空白。
  // 主题与强调色由 index.html 的首帧注入先顶上，loadAppConfig 回来后再 hydrate 覆盖。
  adoptDetachedSession({
    backendId: meta.sessionId,
    label: meta.label,
    user: meta.user,
    port: meta.port,
    host: meta.host,
  })
  showToolbox.value = true
  ready.value = true
  ;(window as any).__hideSplash?.()

  try {
    // 与主窗口同一条启动路径：settings / keys 等节 hydrate 之后，
    // 主题、强调色、终端配色与公钥下拉才和主窗口一致。
    // 3 秒还没回来就先让界面用默认值跑，配置到了再补（不能一直等）
    await Promise.race([
      loadAppConfig(),
      new Promise(resolve => setTimeout(resolve, 3000)),
    ])
  } catch (e) {
    console.error('独立窗口加载配置失败，使用内置默认值：', e)
  }

  // 它盯的那条会话没了（标签关闭、或断线重连换了新会话）：自己收摊，
  // 否则留下一个每个操作都报「会话不存在」的窗口
  await onToolboxSessionGone(meta.sessionId, () => {
    toast('会话已断开或重连，独立窗口即将关闭', 'warn', 1700)
    setTimeout(() => void appWindow().then(w => w?.close()), 1500)
  })
})
</script>

<template>
  <ToolboxModal v-if="ready" detached />
  <div v-else-if="broken" class="dtx-broken">{{ broken }}</div>
  <ConfirmModal />
  <PasswordModal />
  <AppToast />
</template>

<style scoped>
/* 根节点不占布局：工具箱自己 Teleport 到 body 并铺满客户区，
   这里只兜住「注入缺失」这种异常态 */
.dtx-broken {
  height: 100vh;
  display: grid;
  place-items: center;
  padding: 24px;
  text-align: center;
  font-size: 12.5px;
  color: var(--muted);
}
</style>
