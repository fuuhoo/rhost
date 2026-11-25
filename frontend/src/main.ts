import { createApp } from 'vue'
// 终端等宽字体随包发布：不依赖用户系统是否装过这些字体（许可证见 frontend/public 下对应 *.txt）。
// 必须在 style.css 之前，让 @font-face 先于使用它的样式声明生效。
// JetBrains Mono 取字重下拉用到的 300/400/500/700 四档，保证「细体/中等」也是真字形而非合成描边。
import '@fontsource/jetbrains-mono/300.css'
import '@fontsource/jetbrains-mono/400.css'
import '@fontsource/jetbrains-mono/500.css'
import '@fontsource/jetbrains-mono/700.css'
// Cascadia Code（微软 Windows Terminal 字体）在下拉里显示为「Monospace」，只有常规/粗体两档。
import '@fontsource/cascadia-code/400.css'
import '@fontsource/cascadia-code/700.css'
// DejaVu Sans Mono / Liberation Mono 无 @fontsource 包，直接发布官方 TTF，见 fonts.css
import './fonts.css'
import './style.css'
import App from './App.vue'
import { toolboxWindowMeta } from './lib/tauri'

// Splash JS 层兜底：任何加载期错误立即撤掉启动画面（docs/splash-design.md §6）
window.addEventListener('error', () => (window as any).__hideSplash?.())
window.addEventListener('unhandledrejection', () => (window as any).__hideSplash?.())

// 同一份 bundle 两个入口：Rust 建主窗口时注入 __RHOST_BOOT__，建工具箱独立窗口时
// 额外注入 __RHOST_TOOLBOX__（会话信息）。见到它就只挂工具箱，不挂整套主界面——
// 独立窗口没有标签栏/侧栏/终端，挂 App.vue 会白跑一遍主窗口的启动流程。
if (toolboxWindowMeta()) {
  // 不透明底色 + 无 splash 的形态本该由 index.html 头部脚本按首帧注入挂好；这里再补一次，
  // 防的是注入晚于页面脚本执行的情况（那样 #app 会被 splash-active 的 visibility:hidden
  // 一直压住，窗口看起来就是全空的）。
  document.documentElement.classList.add('detached-window')
  document.documentElement.classList.remove('splash-active')
  import('./views/ToolboxWindow.vue')
    .then(m => createApp(m.default).mount('#app'))
    .catch(e => {
      console.error('工具箱独立窗口加载失败：', e)
      // 控制台只有开发版能看，窗口给一句人话，别让它空着
      const box = document.createElement('pre')
      box.style.cssText = 'position:fixed;inset:0;margin:0;padding:22px;overflow:auto;'
        + 'background:#06090d;color:#e6edf3;font:12px/1.75 ui-monospace,Consolas,monospace;'
        + 'white-space:pre-wrap'
      box.textContent = `工具箱独立窗口加载失败。\n\n${e instanceof Error ? e.message : String(e)}`
      document.body.appendChild(box)
      ;(window as any).__hideSplash?.()
    })
} else {
  createApp(App).mount('#app')
}
