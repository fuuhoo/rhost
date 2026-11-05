import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import { invoke } from '@tauri-apps/api/core'
import { isTauri } from './tauri'
import { openExternalSession, type ExternalLinkInput } from '../stores/session'
import { toast } from '../composables/useToast'

/** 后端 external::OpenItem（camelCase）：一条可打开的链接，或被拒链接的原因 */
interface OpenItem {
  link?: ExternalLinkInput
  error?: string
}

/** 事件名与后端 `external::dispatch` 的 emit 对齐；载荷恒为空，真身在队列里 */
const OPEN_EVENT = 'external-open'

function applyItems(items: OpenItem[]) {
  for (const item of items) {
    if (item.link) openExternalSession(item.link)
    else if (item.error) toast(`链接无法打开：${item.error}`, 'err', 6000)
  }
}

/**
 * 堡垒机外部连接的前端入口：订阅后端链接到达事件并领取队列。
 *
 * 「入队 + 事件」双轨（后端 external::dispatch）：命令行/系统 URL 可能在
 * 前端监听器挂载前就到达（冷启动），所以先挂监听、再领取一次积压——
 * take_external_links 是**取空**语义，两条路径重复触发不会重复开会话。
 */
export async function startExternalLinks(): Promise<UnlistenFn | null> {
  if (!isTauri) return null
  const drain = () => {
    invoke<OpenItem[]>('take_external_links')
      .then(applyItems)
      .catch(e => console.error('take_external_links 失败:', e))
  }
  let unlisten: UnlistenFn | null = null
  try {
    unlisten = await listen(OPEN_EVENT, drain)
  } catch (e) {
    console.error('订阅 external-open 失败:', e)
  }
  drain()
  return unlisten
}
