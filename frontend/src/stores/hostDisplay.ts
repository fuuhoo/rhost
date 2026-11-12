import { ref, watch } from 'vue'
import { isTauri } from '../lib/tauri'
import { onConfigLoad, patchUiState } from './appConfig'

/** 主机页是否按分组显示；false = 平铺列表（原「主机」页），true = 分组视角（原「分组」页） */
export const groupedView = ref(false)

/** hydrate 当次赋值不回写 */
let hydrated = false

onConfigLoad(snap => {
  hydrated = false
  groupedView.value = snap.uiState.groupedView === true
  hydrated = true
})

watch(
  groupedView,
  v => {
    if (!isTauri || !hydrated) return
    patchUiState({ groupedView: v })
  },
  { flush: 'sync' },
)
