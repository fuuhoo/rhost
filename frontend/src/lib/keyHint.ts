/**
 * 快捷键的平台差异：存储串 ↔ 平台原生写法。
 *
 * 绑定值统一用「修饰键 + 主键」以 '+' 连接存储（见 lib/shortcuts.ts），
 * 但 mac 习惯用符号（⌘⇧⌥⌃、↵），Windows/Linux 习惯用词（Ctrl/Shift/Alt/Enter）。
 * 本模块是唯一的换算入口：
 * - `fmtCombo` 给提示文案用（tooltip / 右键菜单 / 空态说明）；
 * - `comboParts` 给 <kbd> 键帽分段用；
 * - `toNativeCombo` 给非 mac 平台的旧配置迁移用（把 ⌘+T 落成 Ctrl+T）。
 *
 * 约定：存储串里的 ⌘ 一律表示「主修饰键」，在 mac 上是 Command，
 * 在其它平台落到 Ctrl——与 FinalShell 等跨平台终端的键位习惯一致。
 */
import { detectPlatform } from './tauri'

/** 当前是否 macOS（渲染进程启动时平台已就绪，可模块级求值） */
export const isMac = detectPlatform() === 'macos'

/** 主修饰键的存储写法：mac 用 ⌘，其它平台用 Ctrl 字面量 */
export const MOD = isMac ? '⌘' : 'Ctrl'

/** 回车键的展示标签：mac 用 ↵，Windows/Linux 用 Enter */
export const ENTER_LABEL = isMac ? '↵' : 'Enter'

/** mac 上表示组合键的符号（含主键符号 ↵） */
const MOD_SYMBOLS = ['⌘', '⌥', '⇧', '⌃']

/** 把存储串拆成「修饰键 + 主键」；兼容 '⌘+T' 与紧凑写法 '⌘⇧B' / '⌘↵' */
function splitCombo(raw: string): string[] {
  const s = raw.trim()
  if (!s) return []
  if (s.includes('+')) return s.split('+').map(p => p.trim()).filter(Boolean)
  const parts: string[] = []
  let rest = s
  while (rest && MOD_SYMBOLS.includes(rest[0] as string)) {
    parts.push(rest[0] as string)
    rest = rest.slice(1)
  }
  if (rest) parts.push(rest)
  return parts
}

/** 单个分段的平台标签：修饰键换成当前平台的写法，主键做少量约定替换 */
function labelOf(part: string): string {
  const lower = part.toLowerCase()
  if (part === '⌘' || lower === 'cmd' || lower === 'meta' || lower === 'command') {
    return isMac ? '⌘' : 'Ctrl'
  }
  if (part === '⌃' || lower === 'ctrl' || lower === 'control') return isMac ? '⌃' : 'Ctrl'
  if (part === '⌥' || lower === 'alt' || lower === 'option') return isMac ? '⌥' : 'Alt'
  if (part === '⇧' || lower === 'shift') return isMac ? '⇧' : 'Shift'
  // 主键：回车符号在非 mac 平台写成 Enter，其余原样（字母已是大写）
  if (part === '↵') return isMac ? '↵' : 'Enter'
  if (lower === 'enter' || lower === 'return') return isMac ? '↵' : 'Enter'
  return part
}

/** 提示文案：mac 连写成 ⌘⇧T，其它平台写成 Ctrl+Shift+T */
export function fmtCombo(raw?: string | null): string {
  if (!raw) return ''
  const labels = splitCombo(raw).map(labelOf)
  return isMac ? labels.join('') : labels.join('+')
}

/** <kbd> 分段：每个修饰键单独一枚键帽 */
export function comboParts(raw?: string | null): string[] {
  if (!raw) return []
  return splitCombo(raw).map(labelOf)
}

/** 落盘写法：非 mac 平台把符号换成词并用 '+' 连接（供旧配置迁移），mac 原样返回 */
export function toNativeCombo(raw: string): string {
  if (isMac) return raw
  const labels = comboParts(raw)
  return labels.length ? labels.join('+') : raw
}
