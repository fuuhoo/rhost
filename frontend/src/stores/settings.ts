import { computed, reactive, ref } from 'vue'
import { onConfigLoad, persistSectionNow } from './appConfig'
import { invoke } from '@tauri-apps/api/core'
import { getCurrentWindow } from '@tauri-apps/api/window'
import { isTauri } from '../lib/tauri'
import {
  TERM_BG_BLUE, TERM_SCHEMES, contrastRatio, normalizeTermBg, normalizeTermScheme,
  resolveTermBgImage, srgbChannelLuma, srgbLuminance, type TermPalette,
} from '../lib/termThemes'

/** 设置弹窗显隐（模块级单例，同 showNewConn 模式） */
export const showSettings = ref(false)

export interface EnvVar {
  key: string
  value: string
}

/** 「命令模板」条目：面板里显示 name，选中后把 command 写入当前活动终端并回车执行 */
export interface CommandTemplate {
  name: string
  command: string
}

export interface AppSettings {
  /* ---- 外观 ---- */
  uiTheme: string
  accent: string
  /** 背景不透明度（60~100）：applyGlass() 映射为 --glass-a，驱动外壳玻璃表面 alpha + 原生窗口模糊 */
  opacity: number
  /** 启动动画最短展示时长（ms，200~5000，消费侧夹取）；呼吸动画不受影响，重启生效 */
  splashDurationMs: number
  /** 启动动画背景：true 透明悬浮桌面（需 macOSPrivateApi）/ false 深色底 + LOGO；重启生效 */
  splashTransparent: boolean
  /* ---- 字体 ---- */
  /** 终端等宽字体族名。随包内置四款（无需系统安装）：DejaVu Sans Mono（默认，FinalShell 同款）、
   *  Liberation Mono、JetBrains Mono、Cascadia Code（下拉里显示为「Monospace」）；
   *  'monospace' 表示直接用系统等宽。内置字形见 main.ts 的 @fontsource 导入与 src/fonts.css */
  fontFamily: string
  fontSize: number
  /** 行高百分比（100~200），消费侧 /100 转倍数 */
  lineHeight: number
  /** 字重，持久化为字符串（'300'/'400'/'500'/'700'），消费侧 Number() 转换 */
  fontWeight: string
  /* ---- 终端配色 ---- */
  /** 配色方案 id（一族，含深/浅变体）：fs（FinalShell 经典，默认）/ ink（墨蓝·GitHub 系）/
   *  solarized / gruvbox / catppuccin / tokyo / nord / one / dracula，见 lib/termThemes.ts */
  termTheme: string
  /** 背景颜色：follow（跟随界面主题深浅）/ dark（默认，恒取方案深变体）/ light /
   *  blue（FinalShell 深蓝 #002945）；独立于配色方案——方案决定前景/ANSI，背景由此决定。
   *  默认锁 dark：浅色界面下也要保持 FinalShell 那套深蓝观感（用户明确要求），
   *  且「无浅底」方案在 follow 下会跳到墨蓝浅变体，观感突变 */
  termBg: string
  /** 背景图片开关：开启后 termBgImage 铺在背景色之上，xterm 仍透明 */
  termBgImageOn: boolean
  /** 背景图片（dataURL，本地图片读入后整段持久化；空串 = 未选择） */
  termBgImage: string
  /** 背景图片浓度 20–100（%）：图片层不透明度，越低越向图片自身的平均色压平 */
  termBgImageOpacity: number
  /** 背景图片模糊 0–10（px）：FinalShell bg_img_blur_level 同款，模糊只作用于图片层 */
  termBgImageBlur: number
  /* ---- 终端 ---- */
  /** 登录后为无颜色的远端 shell 注入彩色提示符（PS1 由远端生成，透传不变） */
  colorPrompt: boolean
  /** 连接成功后采集服务器状态绘制 MOTD 面板；开启同时抑制 sshd 原生 MOTD/Last login */
  motd: boolean
  /** 启用自定义 MOTD LOGO：开启在面板首段绘制 motdLogo，关闭不绘制（内容保留） */
  motdLogoOn: boolean
  /** 自定义 MOTD ASCII LOGO（多行文本）；仅 motdLogoOn 开启时生效 */
  motdLogo: string
  scrollback: number
  trimOnCopy: boolean
  pasteGuard: boolean
  rightClick: string
  /** 终端光标样式：block 方块 / bar 竖线 / underline 下划线 */
  cursorStyle: string
  /** 终端光标空闲 1 秒后闪烁 */
  cursorBlink: boolean
  /** 网络异常断开时自动重连（远端主动 exit、手动断开不触发；退避 1s 起指数增长，封顶 30s） */
  autoReconnect: boolean
  /** 自动重连最大尝试次数，0 表示不限次数 */
  autoReconnectMaxAttempts: number
  env: EnvVar[]
  /** 「执行命令…」面板里的自定义命令条目（全局一份，无占位符）；
   *  仅由前端消费（写终端），Rust 侧只负责持久化 */
  commandTemplates: CommandTemplate[]
  /* ---- 快捷键 ---- */
  'key.newTab': string
  'key.newWindow': string
  'key.closeTab': string
  'key.splitV': string
  'key.splitH': string
  'key.clear': string
  'key.palette': string
  'key.find': string
  'key.settings': string
  /* ---- 高级 ---- */
  gpuAccel: boolean
  /** 渲染后端：'auto'（xterm 内置）/ 'webgl'（@xterm/addon-webgl，GPU 加速）；gpuAccel=false 时强制内置 */
  renderer: string
  /* ---- 监控 ---- */
  /** 工具内存采集间隔（秒），驱动底部状态栏内存指标轮询 */
  memInterval: number
  /** 远端主机动态指标（CPU/内存/网络/磁盘/进程/GPU）采集间隔（秒），1~10，后端再夹取 */
  metricsInterval: number
  /** 工作台不可见（回首页/窗口最小化）时暂停内存采集 */
  memPauseHidden: boolean
  /** 内存告警阈值（MB）：主进程 RSS 持续超过该值时状态栏告警 */
  memAlertMb: number
  /** 历史会话保留数量上限（超出后丢弃最旧的记录） */
  recentConnectionsLimit: number
  /* ---- AI 助手 ---- */
  /** AI 功能总开关：关闭后右侧栏隐藏 AI 入口 */
  aiEnabled: boolean
  /** 模型服务商预设：custom 自定义 / openai / deepseek / ollama（仅快捷填充 aiBaseUrl，不锁定） */
  aiProvider: string
  /** 模型服务地址（OpenAI 兼容 API） */
  aiBaseUrl: string
  /** API 密钥（仅保存时一次性使用，真实值存于系统钥匙串；此处常态为空，留空表示不修改） */
  aiApiKey: string
  /** 模型名称 */
  aiModel: string
  /** 启用技能注入（app_config_dir/ai/skills/ 目录下的 .md 文件） */
  aiSkillsEnabled: boolean
  /** 技能禁用名单：fileName 数组，命中即不注入（默认 [] 全部启用） */
  aiSkillsDisabled: string[]
  /** 启用规则注入（app_config_dir/ai/rules/ 目录下的 .md 文件） */
  aiRulesEnabled: boolean
  /** 规则禁用名单：fileName 数组，命中即不注入（默认 [] 全部启用） */
  aiRulesDisabled: string[]
  /** 启用跨会话记忆 */
  aiMemoryEnabled: boolean
  /** 记忆条数上限，超出后淘汰最旧记录 */
  aiMemoryLimit: number
  /** 允许多行命令直接执行：关闭时含多行的命令仅可插入终端人工编辑（M2） */
  aiMultilineExec: boolean
  /** 直接执行确认模式：always 每次确认 / risky 高危命令确认（默认）/ never 不确认（M2） */
  aiExecConfirmMode: 'always' | 'risky' | 'never'
  /** 自定义高危命令关键词：一行一条，直接执行的命令包含即视为高危，纯子串匹配（M2） */
  aiRiskKeywords: string
  /** 执行结果自动回传：开启后，AI 提案命令执行完成时捕获终端输出作为 tool 结果
   *  回传模型，实现自主多步执行；关闭时保持零回传（M2.1 隐私边界） */
  aiAutoReturnOutput: boolean
  /* ---- SFTP 文件传输 ---- */
  /** 传输分块大小（KB）：上传/下载单次读写长度，越大吞吐越高、进度粒度越粗 */
  sftpChunkKb: number
  /** 断点续传：同名文件未传完时从已有偏移续传；关闭后一律全量重传 */
  sftpResume: boolean
  /** 断点校验方式：size 文件大小 / sizeMtime 大小+mtime / sha256 哈希
   *  （逻辑在 src-tauri/src/ssh/sftp.rs resume_check_ok，invoke 时经 policy 传参） */
  sftpResumeCheck: string
  /** 文件覆盖策略：skip 跳过 / overwrite 直接覆盖 / newer 仅源文件更新时覆盖 / ask 每次询问
   *  （逻辑在 src-tauri/src/ssh/sftp.rs decide_conflict；ask 走 SFTP_CONFLICT 协议由 DockPanel 弹窗裁决） */
  sftpOverwritePolicy: string
  /** 上传临时文件机制：先写 `.rhost-tmp` 临时文件、完成后原子重命名（仅上传方向；
   *  下载刻意不走 temp——rename 会换掉用户正打开的本地文件 inode）
   *  （逻辑在 src-tauri/src/ssh/sftp.rs temp_path_of + upload_finalize） */
  sftpUploadTemp: boolean
  /** 保留文件元数据：传输后把源文件 mtime/权限 SETSTAT（上传）或写回（下载）到目标
   *  （逻辑在 src-tauri/src/ssh/sftp.rs upload/download_finalize 的 preserve_meta 分支） */
  sftpPreserveMeta: boolean
  /** 全局最大并发传输任务数（跨所有主机与所有工作台标签页，
   *  计数在 frontend/src/lib/transferLedger.ts，闸门在 DockPanel pumpQueue） */
  sftpGlobalConcurrency: number
  /** 单主机最大并发传输任务数（按会话 = 一条 SFTP 通道的在飞数统计，建议保持 1） */
  sftpHostConcurrency: number
  /** 全局带宽限速 KB/s，0 不限速（同一 host:port + 用户的任务共享漏桶，
   *  计量在 src-tauri/src/ssh/sftp.rs RateMeter/Throttle，经 policy 传参） */
  sftpGlobalRateKb: number
  /** 单任务带宽限速 KB/s，0 不限速（与全局限速同时生效时取更慢的节奏） */
  sftpTaskRateKb: number
  /* ---- 日志 ---- */
  /** 日志采集：关闭后不再采集和显示日志 */
  logCollect: boolean
  /** 日志级别：低于该级别的日志不会被采集（debug/info/warn/error） */
  logLevel: string
  /** 内存中保留的日志行数上限，超出后丢弃最旧的行 */
  logMaxLines: number
  /** 持久化到磁盘：关闭后日志仅存在于内存，退出即丢失 */
  logPersist: boolean
  /** 日志存储路径：会话日志文件的存放目录 */
  logStoragePath: string
  /** 日志文件名模板：${date} 为按切割策略格式化的日期，如 rhost_app_20260102.log */
  logNaming: string
  /** 日志切割策略：daily 按天 / weekly 按周 / monthly 按月 / none 不切割 */
  logRotate: string
  /** 保留的日志文件数上限，超出后删除最旧的文件；0 表示不限制 */
  logMaxFiles: number
  /** 日志文件保留天数，超过后按时间清理旧文件 */
  logRetentionDays: number
  /** 可恢复错误的自动重试最大次数（消费方 frontend/src/components/wb/DockPanel.vue
   *  runTask 的 isTransientError 分支；取消/覆盖冲突/跳过/校验失败不重试） */
  sftpRetryCount: number
  /** 重试前等待毫秒（同上，DockPanel 在重试前 sleep） */
  sftpRetryIntervalMs: number
  /** SFTP 子通道空闲超时秒数，超时自动关闭（SSH 终端不受影响），0 不自动关闭。
   *  前端经 invoke('sftp_set_idle_timeout') 下发，看门狗在 src-tauri/src/ssh/sftp.rs SftpState */
  sftpIdleTimeoutSec: number
  /** 传输完成后对整文件做 SHA256 完整性校验（逻辑在 src-tauri/src/ssh/sftp.rs
   *  upload/download_finalize 的 verify_hash 分支，invoke 时经 policy 传参） */
  sftpVerifyHash: boolean
  /** 上传黑名单 glob，一行一条，命中则跳过不创建任务
   *  （前端消费：DockPanel isBlacklistedName，仅上传方向；只支持 * 与 ?） */
  sftpBlacklist: string
  /** 文件树显示以 . 开头的隐藏文件（本地/远端两侧均生效） */
  sftpShowHidden: boolean
  /** 显示「本地」文件栏。关闭后 SFTP 只剩远端一栏（本机侧的新建/重命名/删除与
   *  双栏分隔线随之隐藏，上传改用拖拽或远端栏的下载目录选择；前端消费：
   *  DockPanel showLocalPane，重新打开即恢复原布局） */
  sftpShowLocalPane: boolean
  /** 外部编辑器命令（右键「用编辑器打开」/ 双击远端文件用）。留空按 code → codium →
   *  cursor → 系统默认 顺序自动探测；填写则按空白拆分并支持引号转义，
   *  文件路径始终作为独立参数传给 execve，不经 shell 展开（前端消费：DockPanel openInEditor，
   *  实际执行见 localfs.rs open_with_editor；设置面板的候选清单来自 localfs.rs list_text_editors，
   *  选中或按路径选择都只是往本字段写一条合法命令） */
  sftpEditorCommand: string
  /* ---- 端口转发（tunnel；引擎见 src-tauri/src/ssh/tunnel.rs，store 见 stores/tunnels.ts） ---- */
  /** SSH 连接建立后自动启用该主机已保存的端口转发规则 */
  tunnelAutoStart: boolean
  /** 本地监听端口被占用时的处理：stop 终止规则 / skip 跳过规则 / next 顺延端口 */
  tunnelPortConflict: string
  /** 转发通道建立失败时的自动重试次数，0 不重试 */
  tunnelRetryCount: number
  /** 转发通道中断后自动重建 */
  tunnelReconnect: boolean
  /** 通道保活间隔（秒）——TODO 未消费：主连接 RTT 心跳（30s 周期）已覆盖 NAT 保活作用 */
  tunnelKeepaliveSec: number
  /** 单条转发连接双向空闲自动断开（秒），0 不断开；建连时快照给后端，改动不影响在传连接 */
  tunnelIdleTimeoutSec: number
  /** 动态转发（SOCKS）域名解析位置：local 本地 / remote 远端 */
  tunnelDnsResolve: string
  /* ---- 关于 ---- */
  autoUpdate: boolean
  updateChannel: string
}

export const DEFAULT_SETTINGS: AppSettings = {
  uiTheme: 'auto',
  accent: '#3ddc84',
  opacity: 96,
  splashDurationMs: 400,
  splashTransparent: true,

  fontFamily: 'DejaVu Sans Mono',
  fontSize: 14,
  lineHeight: 125,
  fontWeight: '400',
  termTheme: 'fs',
  termBg: 'dark',
  termBgImageOn: false,
  termBgImage: '',
  termBgImageOpacity: 100,
  termBgImageBlur: 0,

  colorPrompt: true,
  motd: true,
  motdLogoOn: false,
  motdLogo: '',
  scrollback: 10000,
  trimOnCopy: true,
  pasteGuard: false,
  rightClick: 'menu',
  cursorStyle: 'block',
  cursorBlink: true,
  autoReconnect: true,
  autoReconnectMaxAttempts: 5,
  env: [
    { key: 'EDITOR', value: 'nvim' },
    { key: 'LANG', value: 'zh_CN.UTF-8' },
  ],
  // 默认不预置命令模板：自定义命令直接写入远端执行，预置样例只会变成用户的删除工作量
  commandTemplates: [],

  'key.newTab': '⌘+T',
  // 与 FinalShell 默认键位一致（Ctrl+Shift+T 新建终端窗口）
  'key.newWindow': '⌘+Shift+T',
  'key.closeTab': '⌘+W',
  'key.splitV': '⌘+D',
  'key.splitH': '⌘+E',
  'key.clear': '⌘+L',
  'key.palette': '⌘+K',
  'key.find': '⌘+F',
  'key.settings': '⌘+,',

  gpuAccel: true,
  renderer: 'auto',

  memInterval: 2,
  metricsInterval: 2,
  memPauseHidden: true,
  memAlertMb: 300,
  recentConnectionsLimit: 20,

  aiEnabled: true,
  aiProvider: 'custom',
  aiBaseUrl: '',
  aiApiKey: '',
  aiModel: '',
  aiSkillsEnabled: true,
  aiSkillsDisabled: [],
  aiRulesEnabled: true,
  aiRulesDisabled: [],
  aiMemoryEnabled: true,
  aiMemoryLimit: 100,
  aiMultilineExec: false,
  aiExecConfirmMode: 'risky',
  aiRiskKeywords: '',
  aiAutoReturnOutput: false,

  sftpChunkKb: 64,
  sftpResume: true,
  sftpResumeCheck: 'size',
  sftpOverwritePolicy: 'newer',
  sftpUploadTemp: true,
  sftpPreserveMeta: false,
  sftpGlobalConcurrency: 3,
  sftpHostConcurrency: 1,
  sftpGlobalRateKb: 0,
  sftpTaskRateKb: 0,

  logCollect: true,
  logLevel: 'info',
  logMaxLines: 5000,
  logPersist: true,
  logStoragePath: '',
  logNaming: 'rhost_app_${date}.log',
  logRotate: 'daily',
  logMaxFiles: 100,
  logRetentionDays: 30,
  sftpRetryCount: 3,
  sftpRetryIntervalMs: 1000,
  sftpIdleTimeoutSec: 300,
  sftpVerifyHash: false,
  sftpBlacklist: '.DS_Store\nThumbs.db',
  sftpShowHidden: false,
  sftpShowLocalPane: false,
  sftpEditorCommand: '',

  tunnelAutoStart: false,
  tunnelPortConflict: 'stop',
  tunnelRetryCount: 3,
  tunnelReconnect: true,
  tunnelKeepaliveSec: 15,
  tunnelIdleTimeoutSec: 0,
  tunnelDnsResolve: 'local',

  autoUpdate: true,
  updateChannel: 'stable',
}

/* ---- 持久化：后端 app_config.json settings 节（唯一真相源，localStorage 已下线） ---- */

function cloneDefaults(): AppSettings {
  return JSON.parse(JSON.stringify(DEFAULT_SETTINGS)) as AppSettings
}

/** 启动初始/后端不可用时为默认值；loadAppConfig 后由快照 hydrate 覆盖 */
export const savedSettings = reactive<AppSettings>(cloneDefaults())

/** settings 节（非 log* 键，后端 camelCase 原样）灌入内存态 */
function applySettingsSection(section: Record<string, unknown>) {
  // 旧模型二段迁移：①更早的平铺方案 id，'light' 专指「浅底」→ 方案 ink + 浅色背景；
  // ②旧「明暗」termMode（auto/dark/light）→ 新「背景颜色」termBg。
  // auto 不能一律映射成 follow：只有自带浅色变体的方案，旧 auto 在浅色界面下
  // 真的渲染过浅底；「无浅底」方案（如 fs）旧 auto 恒渲染深变体，
  // 映射成 follow 会让它突然跳到墨蓝浅底，观感大变。
  if (typeof section.termBg !== 'string') {
    const mode = section.termMode
    if (mode === 'dark' || mode === 'light') {
      section.termBg = mode
    } else {
      const scheme = TERM_SCHEMES[normalizeTermScheme(String(section.termTheme ?? ''))]
      section.termBg = scheme.light ? 'follow' : 'dark'
    }
  }
  if (section.termTheme === 'light' && typeof section.termMode !== 'string') section.termBg = 'light'
  delete section.termMode
  Object.assign(savedSettings, section)
}

/** logs 节字段（磁盘 snake_case，与后端 LogsSection 序列化一致）→ savedSettings 的 log* 字段；
 *  类型一致才采纳，拒绝手改脏值。注意不要按 LogConfigPayload 的 camelCase 读——
 *  快照透传的是落盘形态（snake_case），曾因此导致 storagePath/maxLines 等多词字段回填丢失 */
function applyLogsSection(logs: Record<string, unknown>) {
  const mapping: Array<[string, keyof AppSettings]> = [
    ['collect', 'logCollect'],
    ['level', 'logLevel'],
    ['max_lines', 'logMaxLines'],
    ['persist', 'logPersist'],
    ['storage_path', 'logStoragePath'],
    ['rotate', 'logRotate'],
    ['max_files', 'logMaxFiles'],
    ['retention_days', 'logRetentionDays'],
  ]
  for (const [from, to] of mapping) {
    const v = logs[from]
    if (v !== undefined && v !== null && typeof v === typeof savedSettings[to]) {
      // log* 字段仅 string/boolean/number 三类，typeof 窄化后赋值安全
      ;(savedSettings as Record<string, unknown>)[to] = v
    }
  }
}

// 快照到达/导入刷新时 hydrate（注册在模块顶层，import 即生效）
onConfigLoad(snap => {
  applySettingsSection(snap.settings)
  applyLogsSection(snap.logs)
  applyAccent(savedSettings.accent)
  applyTheme()
  applyGlass()
})

/**
 * 写穿设置：log* 键由调用方经 syncLogConfig（set_log_config）通道处理，
 * 本函数只写 settings 节；schema 校验失败后端拒绝且零写入。
 */
export function persistSettings() {
  const all = JSON.parse(JSON.stringify(savedSettings)) as Record<string, unknown>
  // log* 属 logs 节，绝不写入 settings（后端 SettingsSection 亦无这些字段）
  for (const k of Object.keys(all)) {
    if (k.startsWith('log')) delete all[k]
  }
  persistSectionNow('settings', all).catch(e => console.error('设置写穿失败:', e))
}

/** 强调色应用到全局（view-card 等处 var(--accent) 生效）。
 *  同时注入 --accent-rgb（"r,g,b"）供 rgba() 派生半透明色（如日志搜索高亮），
 *  避免依赖 color-mix（老版本 WKWebView 不支持）。 */
export function applyAccent(color: string) {
  const root = document.documentElement.style
  root.setProperty('--accent', color)
  const rgb = hexToRgbTriplet(color)
  if (rgb) root.setProperty('--accent-rgb', rgb)
}

/** #rgb / #rrggbb → "r,g,b"；非法输入返回 null */
function hexToRgbTriplet(hex: string): string | null {
  const m = /^#?([0-9a-fA-F]{3}|[0-9a-fA-F]{6})$/.exec(hex.trim())
  if (!m) return null
  let h = m[1]
  if (h.length === 3) h = h.split('').map(c => c + c).join('')
  const n = parseInt(h, 16)
  return `${(n >> 16) & 255},${(n >> 8) & 255},${n & 255}`
}
applyAccent(savedSettings.accent)

/* ---- 界面主题：dark / light / auto（跟随系统） ---- */

/** uiTheme 经 auto 解析后的实际主题；xterm 配色、MOTD 等订阅此值 */
export type ResolvedTheme = 'dark' | 'light'
export const resolvedTheme = ref<ResolvedTheme>('dark')

const themeMedia =
  typeof window !== 'undefined' && typeof window.matchMedia === 'function'
    ? window.matchMedia('(prefers-color-scheme: dark)')
    : null

/** uiTheme 字符串 → 实际主题；auto/非法值跟随系统（无 matchMedia 的环境视为深色） */
export function resolveUiTheme(v: string): ResolvedTheme {
  if (v === 'light') return 'light'
  if (v === 'dark') return 'dark'
  return themeMedia?.matches === false ? 'light' : 'dark'
}

/**
 * 应用界面主题：html[data-theme] 驱动 style.css 浅色变量层（style.css :root 为深色默认）。
 * 传原始 uiTheme 给后端：auto 时不设原生主题（窗口天然跟随系统外观），
 * 显式 dark/light 时锁定原生外观（macOS 红绿灯/毛玻璃底色随 NSAppearance 联动）。
 * index.html 头部脚本已按 __RHOST_BOOT__/系统外观先行设过 data-theme 防首帧闪烁，
 * 此处为一致性兜底（值相同则 DOM 无变化）。
 * `forced`：Tauri 原生主题事件的权威值——「跟随系统」时 WKWebView 的 matchMedia
 * 在 macOS 上不可靠（切换系统外观常常不派发 change），原生事件 payload 即为
 * 实际主题，直接采用，避免被陈旧的 matchMedia 值覆盖。
 */
export function applyTheme(forced?: 'dark' | 'light') {
  const t = forced ?? resolveUiTheme(savedSettings.uiTheme)
  resolvedTheme.value = t
  document.documentElement.dataset.theme = t
  if (isTauri) {
    invoke('set_window_theme', { theme: savedSettings.uiTheme }).catch((e: unknown) =>
      console.error('set_window_theme 失败:', e),
    )
  }
}

// auto 模式下系统外观切换时跟随（模块级监听，生命周期与页面一致）
themeMedia?.addEventListener('change', () => {
  if (resolveUiTheme(savedSettings.uiTheme) !== resolvedTheme.value) applyTheme()
})

// Tauri 原生主题事件兜底：macOS WKWebView 的 prefers-color-scheme change
// 事件不可靠（Tauri 已知问题，见 tauri-apps/tauri#9341 一类讨论），切系统外观
// 时 matchMedia 可能不派发；onThemeChanged 由 tao 监听 AppleInterfaceTheme
// 变更派发，macOS/Windows 可靠。仅 auto 模式响应，显式锁定时主题不再跟随。
if (isTauri) {
  void getCurrentWindow().onThemeChanged(({ payload }) => {
    if (savedSettings.uiTheme === 'dark' || savedSettings.uiTheme === 'light') return
    if (payload === 'dark' || payload === 'light') applyTheme(payload)
  })
}

/* ---- 终端配色：方案（一族深/浅变体）× 背景颜色（独立设置） ---- */

/**
 * 方案 × 背景 → 配色。背景独立于方案（设置 → 终端 → 背景颜色）：
 * - follow：跟随界面主题深浅取变体（浅色界面取浅变体）；
 * - dark / light：锁定深/浅变体；
 * - blue：恒为 FinalShell 深蓝底（#002945）+ 深色变体配色（FinalShell 默认观感）；
 * 方案无浅色变体而背景为浅（light 或浅色界面）时，回落墨蓝浅色变体——
 * 深色变体的白前景在浅底上不可读（fs 这类「无浅底」方案即属此列）。
 */
export function pickTermPalette(id: string, bg: string = savedSettings.termBg): TermPalette {
  const scheme = TERM_SCHEMES[normalizeTermScheme(id)]
  const light = scheme.light ?? TERM_SCHEMES.ink.light!
  switch (normalizeTermBg(bg)) {
    case 'dark': return scheme.dark
    case 'light': return light
    case 'blue': return { ...scheme.dark, background: TERM_BG_BLUE, cursorAccent: TERM_BG_BLUE }
    default: return resolvedTheme.value === 'light' ? light : scheme.dark
  }
}

/** 已测出的背景图片统计（平均相对亮度 + 平均色；按地址缓存，没测出来就不写键） */
type BgImageStats = { luma: number; rgb: [number, number, number] }
const bgImageStats = reactive<Record<string, BgImageStats>>({})
const bgImageProbing = new Set<string>()

/** 缩到 32×32 采样：一次遍历同时得到平均相对亮度（定墨色档）与平均色（做图片衬底）。
 *  解码失败或画布被污染时不写缓存，墨色沿用「背景颜色」设置、衬底留透明。 */
function probeBgImage(url: string) {
  if (bgImageProbing.has(url)) return
  bgImageProbing.add(url)
  const img = new Image()
  img.onload = () => {
    try {
      const canvas = document.createElement('canvas')
      canvas.width = canvas.height = 32
      const ctx = canvas.getContext('2d')
      if (!ctx) return
      ctx.drawImage(img, 0, 0, 32, 32)
      const px = ctx.getImageData(0, 0, 32, 32).data
      const n = px.length / 4
      let luma = 0
      let r = 0
      let g = 0
      let b = 0
      for (let i = 0; i < px.length; i += 4) {
        luma += 0.2126 * srgbChannelLuma(px[i]) + 0.7152 * srgbChannelLuma(px[i + 1]) + 0.0722 * srgbChannelLuma(px[i + 2])
        r += px[i]
        g += px[i + 1]
        b += px[i + 2]
      }
      bgImageStats[url] = { luma: luma / n, rgb: [Math.round(r / n), Math.round(g / n), Math.round(b / n)] }
    } catch { /* 取不到像素：保持未测出 */ }
  }
  img.onerror = () => bgImageProbing.delete(url)
  img.src = url
}

/** 图片解析后的地址；未测出时顺手发起测量，测到后 computed 自动重算 */
function bgImageStatsFor(img: string): BgImageStats | undefined {
  const url = resolveTermBgImage(img)
  const stats = bgImageStats[url]
  if (stats === undefined) probeBgImage(url)
  return stats
}

/**
 * 开了背景图片时「背景颜色」不再参与——图片就是衬底，但图片层按浓度半透明，
 * 底下垫什么就决定观感：垫窗口玻璃（浅色界面近乎纯白）会把深蓝壁纸洗成灰膜，
 * 所以垫图片自身的平均色。浓度调低于是变成「向自己的主色压平」，而不是发白。
 * 未测出时返回 null，调用方回落透明。
 */
export function termBgImageTint(
  img: string = savedSettings.termBgImage,
  imgOn: boolean = savedSettings.termBgImageOn,
): string | null {
  if (!imgOn || !img) return null
  const stats = bgImageStatsFor(img)
  return stats ? `rgb(${stats.rgb.join(' ')})` : null
}

/**
 * 墨色实际用的背景取值。开了背景图片时「背景颜色」不再参与——图片就是衬底，
 * 故在深/浅两套变体里挑与图片平均亮度对比度更高的那套（FinalShell 的白字压深蓝
 * 壁纸就是这么来的，但这里连浅壁纸也一起兜住：壁纸亮就换回深色墨）。
 * 图片地址是内置引用还是 dataURL 都按解析后的地址缓存，换图即重测。
 */
export function termInkBg(
  themeId: string = savedSettings.termTheme,
  bg: string = savedSettings.termBg,
  imgOn: boolean = savedSettings.termBgImageOn,
  img: string = savedSettings.termBgImage,
): string {
  if (!imgOn || !img) return bg
  const stats = bgImageStatsFor(img)
  if (stats === undefined) return bg // 首帧先按设置项出图，测出来后 computed 自动重算
  const onDark = contrastRatio(stats.luma, srgbLuminance(pickTermPalette(themeId, 'dark').foreground))
  const onLight = contrastRatio(stats.luma, srgbLuminance(pickTermPalette(themeId, 'light').foreground))
  return onDark >= onLight ? 'dark' : 'light'
}

/** 当前生效的终端配色（xterm theme、容器底色、状态面板 SGR、查找装饰共用一份） */
export const termPalette = computed(() => pickTermPalette(savedSettings.termTheme, termInkBg()))

/* ---- 背景不透明度：60–100 → 玻璃表面 alpha + 原生窗口模糊 ---- */

/** 归一到合法区间 60–100；非法输入回落默认值（与设置项 range 一致） */
export function clampOpacity(v: unknown): number {
  return typeof v === 'number' && Number.isFinite(v) ? Math.min(100, Math.max(60, Math.round(v))) : 96
}

/** 背景图片浓度归一 20–100（%）；非法输入回落默认 60（与设置项 range 一致） */
export function clampBgImgOpacity(v: unknown): number {
  return typeof v === 'number' && Number.isFinite(v) ? Math.min(100, Math.max(20, Math.round(v))) : 100
}

/** 背景图片模糊归一 0–10（px）；非法输入回落默认 0（与设置项 range 一致） */
export function clampBgImgBlur(v: unknown): number {
  return typeof v === 'number' && Number.isFinite(v) ? Math.min(10, Math.max(0, Math.round(v))) : 0
}

/**
 * 应用背景不透明度：--glass-a 驱动 style.css 的 --glass-* 外壳表面（body/标题栏/
 * 侧栏/标签栏/Dock/状态栏/终端/右侧栏），弹窗等信息层不透明以保可读性。
 * 同时通知后端按需在窗口底层挂 macOS 模糊材质（<100 才挂，=100 撤除）。
 * index.html 已按 __RHOST_BOOT__.opacity 首帧前设过，此处为热更新与一致性兜底。
 */
export function applyGlass() {
  const o = clampOpacity(savedSettings.opacity)
  document.documentElement.style.setProperty('--glass-a', String(o / 100))
  if (isTauri) {
    invoke('set_window_glass', { opacity: o }).catch((e: unknown) =>
      console.error('set_window_glass 失败:', e),
    )
  }
}

export function saveSettings(draft: AppSettings) {
  const next = JSON.parse(JSON.stringify(draft)) as AppSettings
  Object.assign(savedSettings, next)
  persistSettings()
  applyAccent(savedSettings.accent)
  applyTheme()
  applyGlass()
}

/** 等宽字体回退链：拉丁等宽逐级回退，再补中文（内置字体都无 CJK 字形，
 *  缺这一步中文会掉到浏览器默认字体，字号与基线都可能不一致），最后落通用等宽 */
const MONO_FALLBACK = [
  'ui-monospace', 'SFMono-Regular', 'Menlo', 'Consolas', 'Cascadia Mono',
  // Powerline 私有区字形（U+E0B0…）内置字体与系统自带等宽都没有：装了打补丁字体的
  // 用户按字符回退到它们，agnoster / powerlevel10k 之类提示符才不是一片豆腐块
  '"JetBrainsMono Nerd Font"', '"MesloLGS NF"', '"DejaVu Sans Mono for Powerline"',
  '"Sarasa Mono SC"', '"Source Han Mono SC"', '"Noto Sans Mono CJK SC"',
  '"PingFang SC"', '"Microsoft YaHei"', '"Noto Sans CJK SC"',
  'monospace',
].join(', ')

/**
 * 设置中的字体名 → 可直接用于 CSS / xterm fontFamily 的字体栈。
 * 'monospace' 表示系统等宽（不前置具名字体）；其余值作为首选字体加引号，
 * 防止含空格/数字的字体名被解析成多个族名。设置面板预览与终端共用此函数。
 */
export function resolveTerminalFontFamily(name: string): string {
  return name === 'monospace' ? MONO_FALLBACK : `"${name}", ${MONO_FALLBACK}`
}
