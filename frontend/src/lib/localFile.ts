import { invoke } from '@tauri-apps/api/core'

/** 标准 base64 → 字节：atob 解出的是逐字符 latin1 串，码点即字节值 */
export function base64ToBytes(b64: string): Uint8Array<ArrayBuffer> {
  const bin = atob(b64)
  const out = new Uint8Array(bin.length)
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i)
  return out
}

/** 路径末段文件名（同时兼容 POSIX 与 Windows 分隔符） */
export function pathBaseName(path: string): string {
  return path.split(/[\\/]/).pop() || path
}

/**
 * 按绝对路径读回本机文件并重组为 File。
 * webview 里的原生对话框与系统拖放都只给路径不给字节，MIME 也只能由扩展名判定，
 * 内容统一由后端 read_local_file_base64 读回（超限/非普通文件由它报错）。
 */
export async function readFileFromPath(path: string, type: string): Promise<File> {
  const b64 = await invoke<string>('read_local_file_base64', { path })
  return new File([base64ToBytes(b64)], pathBaseName(path), { type })
}
