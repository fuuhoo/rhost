import { ref } from 'vue'
import { isTauri } from '../lib/tauri'
import { onConfigLoad, patchUiState } from './appConfig'

/** 首页主区可切换的视图；新增页面时在此扩展联合类型 */
export type HomeViewId = 'quick-connect' | 'hosts' | 'keys' | 'logs'

const VIEW_IDS: HomeViewId[] = ['quick-connect', 'hosts', 'keys', 'logs']

/** 模块级单例；Tauri 下持久化于后端 ui_state.homeView */
export const homeView = ref<HomeViewId>('quick-connect')

onConfigLoad(snap => {
  const saved = snap.uiState.homeView
  // 分组页已并入主机页：旧配置里的 groups 一次性迁移到主机页
  if (saved === 'groups') {
    homeView.value = 'hosts'
    if (isTauri) patchUiState({ homeView: 'hosts' })
    return
  }
  if (typeof saved === 'string' && VIEW_IDS.includes(saved as HomeViewId)) {
    homeView.value = saved as HomeViewId
  }
})

export function setHomeView(id: HomeViewId) {
  homeView.value = id
  if (isTauri) patchUiState({ homeView: id })
}
