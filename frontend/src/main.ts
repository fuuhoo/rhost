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

// Splash JS 层兜底：任何加载期错误立即撤掉启动画面（docs/splash-design.md §6）
window.addEventListener('error', () => (window as any).__hideSplash?.())
window.addEventListener('unhandledrejection', () => (window as any).__hideSplash?.())

createApp(App).mount('#app')
