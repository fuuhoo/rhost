/**
 * 跨标签页共享的「传输在飞」登记表。
 *
 * SFTP 并发上限（设置 → SFTP → 全局最大并发）是**客户端级**约束，但每个工作台标签
 * 各有一个 DockPanel 实例和各自的任务队列；只靠单个面板自己的数组统计，两个标签页
 * 会各跑各的上限，实际并发是设置值的 N 倍。故把在飞任务登记在本模块（模块级单例，
 * 所有面板共享），pumpQueue 据此决定还能不能启下一个。
 *
 * 键为 `{backendId}#{taskId}`：backendId 是会话（= 一条 SFTP 通道）的唯一 id，
 * taskId 是该面板内的自增序号，两者拼接后跨标签页唯一。任务到达任一终态
 * （完成 / 跳过 / 失败 / 取消）时由 runTask 的收尾回调清除；会话断开会让在飞的
 * invoke 一并失败并走同一收尾，故不会留下永久占用额度的死条目。
 */
const running = new Map<string, string>()

export function markTransferRunning(backendId: string, taskId: number): void {
  running.set(`${backendId}#${taskId}`, backendId)
}

export function clearTransferRunning(backendId: string, taskId: number): void {
  running.delete(`${backendId}#${taskId}`)
}

/** 指定会话（一条 SFTP 通道）的在飞任务数 —— 对应「单主机最大并发」 */
export function runningOnBackend(backendId: string): number {
  let n = 0
  for (const owner of running.values()) {
    if (owner === backendId) n++
  }
  return n
}

/** 全局在飞任务数（跨所有标签页）—— 对应「全局最大并发传输任务」 */
export function runningTotal(): number {
  return running.size
}
