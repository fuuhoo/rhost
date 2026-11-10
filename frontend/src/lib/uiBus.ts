/**
 * 跨组件的 UI 总线：不归属任何 store 领域、仅需单向唤起的全局状态。
 * 计数总线（xxxTick）沿用 session.ts `searchTick` 模式：发起方 ++，消费方 watch 后聚焦自身控件。
 */
import { ref } from 'vue'

/** 全局命令面板显隐（模块级单例，同 showNewConn / showSettings 模式） */
export const showPalette = ref(false)

/** 工具箱弹窗显隐（顶栏按钮唤起，覆盖当前会话的防火墙/端口工具） */
export const showToolbox = ref(false)

/** 终端查找唤起信号（标题栏按钮与 ⌘F 快捷键共用；TerminalPane 监听后展开并聚焦查找条） */
export const termFindTick = ref(0)

/** 设置弹窗的目标页签：其他入口（命令面板空态行）写入页签 id 后打开设置，
 *  SettingsModal 在打开时消费并清空；为 null 时按常规回到「外观」 */
export const settingsFocusPanel = ref<string | null>(null)
