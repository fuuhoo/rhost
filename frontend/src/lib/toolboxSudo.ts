/**
 * 工具箱 sudo 口令缓存：只存在于前端内存，按后端会话 id 隔离。
 *
 * - 用户在弹窗里输入一次后，同一会话内的后续提权操作复用；
 * - 会话断开/关闭/重连时由 session store 显式清理；
 * - 绝不写入 localStorage、配置或日志。
 */
const cache = new Map<string, string>()

export function cachedSudo(sessionId: string): string | undefined {
  return cache.get(sessionId)
}

export function rememberSudo(sessionId: string, password: string) {
  cache.set(sessionId, password)
}

export function forgetSudo(sessionId: string | undefined) {
  if (sessionId) cache.delete(sessionId)
}

export function clearSudoCache() {
  cache.clear()
}
