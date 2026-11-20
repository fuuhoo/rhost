/**
 * 检查更新：读 GitHub Releases 的最新发布并比对版本，只回答「有没有新版本」，
 * 不下载、不安装。发布产物目前是 ad-hoc 未签名包（见 docs/explanation/design/ci-release-design.md），
 * 应用内自动更新要等签名产物与 tauri-updater 接入后再在此扩展。
 */

const REPO = 'fuuhoo/rhost'
const API_LATEST = `https://api.github.com/repos/${REPO}/releases/latest`

export const repoUrl = `https://github.com/${REPO}`
export const releasesUrl = `${repoUrl}/releases`

export interface ReleaseInfo {
  /** 去掉 `v` 前缀的版本号，如 `2.11.0` */
  version: string
  /** 发布标题，仓库未填时为空串 */
  title: string
  /** 发布页地址 */
  url: string
  /** 发布时间（ISO 8601），缺失时为空串 */
  publishedAt: string
}

interface GithubRelease {
  tag_name?: string
  name?: string
  html_url?: string
  published_at?: string
}

/** `v2.11.0` / `2.11.0-beta.1` / `2.11` → `[2, 11, 0]`，缺失或非数字段按 0 计 */
function versionParts(v: string): number[] {
  const nums = v.trim().replace(/^v/i, '').split(/[.+-]/).slice(0, 3)
  return [0, 1, 2].map(i => {
    const n = Number.parseInt(nums[i] ?? '', 10)
    return Number.isFinite(n) ? n : 0
  })
}

/** `latest` 是否比 `current` 新；相同或更旧返回 false（预发布后缀不参与比较） */
export function isNewer(latest: string, current: string): boolean {
  const a = versionParts(latest)
  const b = versionParts(current)
  for (let i = 0; i < 3; i++) {
    if (a[i] !== b[i]) return a[i] > b[i]
  }
  return false
}

/** 取最新正式发布；网络不可达、限流、仓库无发布等情况抛出具名错误，由调用方展示 */
export async function fetchLatestRelease(signal?: AbortSignal): Promise<ReleaseInfo> {
  let res: Response
  try {
    res = await fetch(API_LATEST, {
      headers: { Accept: 'application/vnd.github+json' },
      signal,
    })
  } catch (e) {
    if ((e as { name?: string } | null)?.name === 'AbortError') throw e
    throw new Error('网络不可达，请检查网络或代理后重试')
  }

  if (res.status === 404) throw new Error('仓库还没有发布正式版本')
  if (res.status === 403 || res.status === 429) throw new Error('请求过于频繁，请稍后再试')
  if (!res.ok) throw new Error(`请求失败（HTTP ${res.status}）`)

  let data: GithubRelease
  try {
    data = (await res.json()) as GithubRelease
  } catch {
    throw new Error('返回内容无法解析')
  }

  const tag = data.tag_name?.trim()
  if (!tag) throw new Error('返回内容缺少版本号')

  return {
    version: tag.replace(/^v/i, ''),
    title: data.name?.trim() ?? '',
    url: data.html_url || releasesUrl,
    publishedAt: data.published_at ?? '',
  }
}
