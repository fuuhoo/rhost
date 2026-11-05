import { getCurrentWebview } from '@tauri-apps/api/webview'
import { detectPlatform, isTauri } from './tauri'

/** 拖放落点（Tauri 事件的单位见 dropToCss） */
export type DropPosition = { x: number; y: number }

/**
 * 落点单位按平台分叉，这是 wry 的实现细节而不是约定：
 * macOS 取 `NSDraggingInfo.draggingLocation`（NSView 的点，即 CSS 逻辑像素，
 * 再用 frame 高度翻成左上原点）；Windows 取 `ScreenToClient`（物理像素）。
 * tauri-runtime-wry 把这两处的数值原样塞进 `PhysicalPosition`，名字骗人，
 * 所以只有 Windows 需要按 devicePixelRatio 折算——统一乘 DPR 会让 macOS 的
 * 命中区凭空放大一倍，落在文件栏正中间的拖拽被判成「落在栏外」。
 */
function dropPosIsPhysical(): boolean {
  return detectPlatform() === 'windows'
}

/** 把落点归一到 CSS 像素（与 getBoundingClientRect 同一坐标系） */
export function dropToCss(pos: DropPosition): DropPosition {
  if (!dropPosIsPhysical()) return pos
  const dpr = window.devicePixelRatio || 1
  return { x: pos.x / dpr, y: pos.y / dpr }
}

/** 落点是否落在该元素矩形内。元素隐藏/收起时矩形宽高为 0，自然不命中。 */
export function dropOverElement(el: HTMLElement | null, pos: DropPosition): boolean {
  if (!el) return false
  const r = el.getBoundingClientRect()
  if (r.width <= 0 || r.height <= 0) return false
  const p = dropToCss(pos)
  return p.x >= r.left && p.x <= r.right && p.y >= r.top && p.y <= r.bottom
}

/**
 * 订阅 webview 级系统文件拖放，只在落在 `zone()` 区域内时回调 `onDrop`。
 *
 * webview 里的 HTML5 drop 拿不到被拖入项的真实路径，路径只能由这个事件给出；
 * 而同一时刻二者只能有一个生效（Tauri 的拖放处理器开启时 HTML5 收不到 File），
 * 所以浏览器预览模式不订阅，由各消费方保留自己的 HTML5 通道。
 *
 * `onMissed` 给出「拖放事件到了但落点不在本区」这一支：不传则与「事件根本没到」
 * 无从区分，排查拖拽是否生效时必须靠它。
 *
 * 返回取消订阅函数；注册本身是异步的，注册完成前取消也不会留下游离回调。
 */
export function watchFileDrop(
  zone: () => HTMLElement | null,
  onHover: (inside: boolean) => void,
  onDrop: (paths: string[]) => void,
  onMissed?: (paths: string[], pos: DropPosition) => void,
): () => void {
  if (!isTauri) return () => {}
  let disposed = false
  let unlisten: (() => void) | null = null
  void getCurrentWebview().onDragDropEvent((e) => {
    const p = e.payload
    if (p.type === 'enter' || p.type === 'over') onHover(dropOverElement(zone(), p.position))
    else if (p.type === 'leave') onHover(false)
    else {
      onHover(false)
      if (dropOverElement(zone(), p.position)) onDrop(p.paths)
      else onMissed?.(p.paths, p.position)
    }
  }).then((fn) => {
    // 监听注册是异步的：若这期间消费方已卸载，直接归还句柄，不留游离回调
    if (disposed) fn()
    else unlisten = fn
  })
  return () => {
    disposed = true
    unlisten?.()
  }
}
