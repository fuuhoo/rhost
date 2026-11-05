/**
 * 终端配色（设置 → 终端）。
 *
 * 两个正交维度解析出一张 TermPalette，同时供三处消费，避免配色分散定义后互相漂移：
 * - 配色方案 termTheme：一族配色，最多有深/浅两个变体，决定前景/16 色 ANSI/光标/选区；
 * - 背景颜色 termBg：跟随界面 / 深色 / 浅色 / 蓝色（FinalShell 深蓝），见 TERM_BG_*；
 * - 背景图片 termBgImage*：可选图片叠在背景色之上（浓度可调），来源分随包内置与本地图，见 TERM_BG_BUILTINS；
 * 消费方：
 * - xterm `options.theme`（前景/光标/选区/16 色 ANSI）；
 * - CSS 变量 `--term-surface`（终端容器底色，走「背景不透明度」玻璃混合）；
 * - MOTD 状态面板的 ANSI SGR（品牌/状态/链接/正文色）与查找高亮装饰。
 *
 * 取色来源（不按截图估色）：
 * - `fs`：逐值取自 FinalShell 内置 Default 终端主题（实测取自其程序包内
 *   finalshell.jar 的 resources/theme/Default.json）；FinalShell 无浅底配色，
 *   浅色背景下回落墨蓝浅色变体（白前景在浅底不可读，见 pickTermPalette）。
 * - `ink`：rhost 自有的低饱和墨蓝，浅底变体走 GitHub Light 系
 *   （浅底用高饱和 ANSI 会糊成一片）。
 * - 其余（Solarized / Gruvbox / Catppuccin / Tokyo Night / Nord / One / Dracula）：
 *   ANSI 16 色、底色、前景取 Alacritty 官方主题仓库 alacritty-theme 的对应 TOML，
 *   光标/选区/查找装饰等该仓库未给定的字段按同一份规则从本族色板推导（见下）。
 *
 * 统一推导规则（新方案照此补齐，勿再各处写死）：
 * - cursor：上游给了就用自己的，没给则取前景（VTE/xterm 默认行为）；
 * - cursorAccent：等于底色，方块光标下的字符退进底色；
 * - selectionBackground：本族 link 色，深色 32% / 浅色 22% alpha；
 * - matchBackground / activeMatchBackground：link 与底色分别按 25%/50%（深）、
 *   18%/42%（浅）混合（SearchAddon 只接受 #RRGGBB，不接受带 alpha 的色值）；
 * - brand / dim / link：品牌色、状态面板正文弱档、链接高亮，优先取本族官方色板里
 *   现成的 token；dim 一律校到与底色 ≥4.5:1（可读性优先于视觉层次）。
 * - 与底色同值或近乎同值的 black/brightBlack（终端里用于背景块、目录色，等于底色就
 *   是隐形）提亮到本族次级灰；上游刻意如此设计的（如 Gruvbox 的 black 就是底色）保留。
 */
export interface TermPalette {
  /** 容器底色；xterm 侧 background 保持透明以透出它 */
  background: string
  foreground: string
  cursor: string
  cursorAccent: string
  selectionBackground: string
  ansi: {
    black: string
    red: string
    green: string
    yellow: string
    blue: string
    magenta: string
    cyan: string
    white: string
    brightBlack: string
    brightRed: string
    brightGreen: string
    brightYellow: string
    brightBlue: string
    brightMagenta: string
    brightCyan: string
    brightWhite: string
  }
  /** MOTD 语义色：品牌/状态面板正文（比正文弱一档）/链接高亮 */
  brand: string
  dim: string
  link: string
  /** 查找装饰色 */
  matchBackground: string
  activeMatchBackground: string
  matchRuler: string
}

/** 一族配色；`light` 缺省表示该方案只有深色变体（明暗选浅色时回落深色） */
export interface TermScheme {
  label: string
  dark: TermPalette
  light?: TermPalette
}

/** 配色方案 id；新增方案时同时补进这里和下面的 TERM_SCHEMES */
export type TermSchemeId = 'fs' | 'ink' | 'solarized' | 'gruvbox' | 'catppuccin' | 'tokyo' | 'nord' | 'one' | 'dracula'

export const TERM_SCHEMES: Record<TermSchemeId, TermScheme> = {
  fs: {
    label: 'FinalShell 经典',
    dark: {
      background: '#002945',
      foreground: '#ffffff',
      cursor: '#00d05c',
      cursorAccent: '#002945',
      selectionBackground: '#05609f',
      ansi: {
        black: '#555555',
        red: '#ff2222',
        green: '#38de21',
        yellow: '#ffe50a',
        blue: '#1460d2',
        magenta: '#ff005d',
        cyan: '#00bbbb',
        white: '#bbbbbb',
        brightBlack: '#555555',
        brightRed: '#f40e17',
        brightGreen: '#3bd01d',
        brightYellow: '#edc809',
        brightBlue: '#5555ff',
        brightMagenta: '#ff99ff',
        brightCyan: '#6ae3fa',
        brightWhite: '#ffffff',
      },
      brand: '#00d05c',
      dim: '#b8c6d4',
      link: '#6ae3fa',
      matchBackground: '#0d4a75',
      activeMatchBackground: '#1a6ea8',
      matchRuler: '#6ae3fa',
    },
  },
  ink: {
    label: '墨蓝（GitHub 系）',
    dark: {
      background: '#0d1a26',
      foreground: '#e6edf3',
      cursor: '#3fb950',
      cursorAccent: '#0d1a26',
      selectionBackground: 'rgba(88, 166, 255, .34)',
      ansi: {
        black: '#4d5c6d',
        red: '#ff7b72',
        green: '#3fb950',
        yellow: '#f0883e',
        blue: '#58a6ff',
        magenta: '#bc8cff',
        cyan: '#39c5cf',
        white: '#b1bac4',
        brightBlack: '#8496a8',
        brightRed: '#ffa198',
        brightGreen: '#56d364',
        brightYellow: '#e3b341',
        brightBlue: '#79c0ff',
        brightMagenta: '#d2a8ff',
        brightCyan: '#56d4dd',
        brightWhite: '#f0f6fc',
      },
      brand: '#39c5cf',
      dim: '#97a3b0',
      link: '#4fd6e0',
      matchBackground: '#17323c',
      activeMatchBackground: '#1f596b',
      matchRuler: '#4fd6e0',
    },
    light: {
      background: '#f7f9fb',
      foreground: '#24292f',
      cursor: '#1a7f37',
      cursorAccent: '#f7f9fb',
      selectionBackground: 'rgba(9, 105, 218, .22)',
      ansi: {
        black: '#24292e',
        red: '#cf222e',
        green: '#1a7f37',
        yellow: '#9a6700',
        blue: '#0550ae',
        magenta: '#8250df',
        cyan: '#1b7c83',
        white: '#6e7781',
        brightBlack: '#57606a',
        brightRed: '#a40e26',
        brightGreen: '#116329',
        brightYellow: '#4d2d00',
        brightBlue: '#0969da',
        brightMagenta: '#8250df',
        brightCyan: '#3192aa',
        brightWhite: '#8c959f',
      },
      brand: '#0891b2',
      dim: '#57606a',
      link: '#0969da',
      matchBackground: '#d4ecf7',
      activeMatchBackground: '#9fd3ee',
      matchRuler: '#0969da',
    },
  },
  solarized: {
    label: 'Solarized',
    dark: {
      background: '#002b36',
      foreground: '#839496',
      cursor: '#839496',
      cursorAccent: '#002b36',
      selectionBackground: 'rgba(38, 139, 210, .32)',
      ansi: {
        black: '#073642',
        red: '#dc322f',
        green: '#859900',
        yellow: '#b58900',
        blue: '#268bd2',
        magenta: '#d33682',
        cyan: '#2aa198',
        white: '#eee8d5',
        // brightBlack 上游给的是 #002b36（=底色，等于隐形），按上面的规则提到 base01
        brightBlack: '#586e75',
        brightRed: '#cb4b16',
        brightGreen: '#586e75',
        brightYellow: '#657b83',
        brightBlue: '#839496',
        brightMagenta: '#6c71c4',
        brightCyan: '#93a1a1',
        brightWhite: '#fdf6e3',
      },
      brand: '#2aa198',
      dim: '#93a1a1',
      link: '#268bd2',
      matchBackground: '#0a435d',
      activeMatchBackground: '#135b84',
      matchRuler: '#268bd2',
    },
    light: {
      background: '#fdf6e3',
      foreground: '#586e75',
      cursor: '#586e75',
      cursorAccent: '#fdf6e3',
      selectionBackground: 'rgba(38, 139, 210, .22)',
      ansi: {
        black: '#073642',
        red: '#dc322f',
        green: '#859900',
        yellow: '#b58900',
        blue: '#268bd2',
        magenta: '#d33682',
        cyan: '#2aa198',
        // Solarized 深浅共用同一组 base：浅底上 base2/base3（近白）当文字等于隐形，
        // 故 white/brightWhite 换成本族次级灰 base1/base02，其余 16 色保持上游值
        white: '#93a1a1',
        brightBlack: '#657b83',
        brightRed: '#cb4b16',
        brightGreen: '#586e75',
        brightYellow: '#657b83',
        brightBlue: '#839496',
        brightMagenta: '#6c71c4',
        brightCyan: '#93a1a1',
        brightWhite: '#073642',
      },
      brand: '#2aa198',
      dim: '#657b83',
      link: '#268bd2',
      matchBackground: '#d6e3e0',
      activeMatchBackground: '#a3c9dc',
      matchRuler: '#268bd2',
    },
  },
  gruvbox: {
    label: 'Gruvbox',
    dark: {
      background: '#282828',
      foreground: '#ebdbb2',
      cursor: '#ebdbb2',
      cursorAccent: '#282828',
      selectionBackground: 'rgba(131, 165, 152, .32)',
      ansi: {
        black: '#282828',
        red: '#cc241d',
        green: '#98971a',
        yellow: '#d79921',
        blue: '#458588',
        magenta: '#b16286',
        cyan: '#689d6a',
        white: '#a89984',
        brightBlack: '#928374',
        brightRed: '#fb4934',
        brightGreen: '#b8bb26',
        brightYellow: '#fabd2f',
        brightBlue: '#83a598',
        brightMagenta: '#d3869b',
        brightCyan: '#8ec07c',
        brightWhite: '#ebdbb2',
      },
      brand: '#fabd2f',
      dim: '#a89984',
      link: '#83a598',
      matchBackground: '#3f4744',
      activeMatchBackground: '#566660',
      matchRuler: '#83a598',
    },
    light: {
      background: '#fbf1c7',
      foreground: '#3c3836',
      cursor: '#3c3836',
      cursorAccent: '#fbf1c7',
      selectionBackground: 'rgba(7, 102, 120, .22)',
      ansi: {
        black: '#fbf1c7',
        red: '#cc241d',
        green: '#98971a',
        yellow: '#d79921',
        blue: '#458588',
        magenta: '#b16286',
        cyan: '#689d6a',
        white: '#7c6f64',
        brightBlack: '#928374',
        brightRed: '#9d0006',
        brightGreen: '#79740e',
        brightYellow: '#b57614',
        brightBlue: '#076678',
        brightMagenta: '#8f3f71',
        brightCyan: '#427b58',
        brightWhite: '#3c3836',
      },
      brand: '#d79921',
      dim: '#7c6f64',
      link: '#076678',
      matchBackground: '#cfd8b9',
      activeMatchBackground: '#95b7a6',
      matchRuler: '#076678',
    },
  },
  catppuccin: {
    label: 'Catppuccin',
    dark: {
      background: '#1e1e2e',
      foreground: '#cdd6f4',
      cursor: '#f5e0dc',
      cursorAccent: '#1e1e2e',
      selectionBackground: 'rgba(137, 180, 250, .32)',
      ansi: {
        black: '#45475a',
        red: '#f38ba8',
        green: '#a6e3a1',
        yellow: '#f9e2af',
        blue: '#89b4fa',
        magenta: '#f5c2e7',
        cyan: '#94e2d5',
        white: '#bac2de',
        brightBlack: '#585b70',
        brightRed: '#f38ba8',
        brightGreen: '#a6e3a1',
        brightYellow: '#f9e2af',
        brightBlue: '#89b4fa',
        brightMagenta: '#f5c2e7',
        brightCyan: '#94e2d5',
        brightWhite: '#a6adc8',
      },
      brand: '#cba6f7',
      dim: '#bac2de',
      link: '#89b4fa',
      matchBackground: '#394461',
      activeMatchBackground: '#546994',
      matchRuler: '#89b4fa',
    },
    light: {
      background: '#eff1f5',
      foreground: '#4c4f69',
      cursor: '#dc8a78',
      cursorAccent: '#eff1f5',
      selectionBackground: 'rgba(30, 102, 245, .22)',
      ansi: {
        black: '#5c5f77',
        red: '#d20f39',
        green: '#40a02b',
        yellow: '#df8e1d',
        blue: '#1e66f5',
        magenta: '#ea76cb',
        cyan: '#179299',
        white: '#acb0be',
        brightBlack: '#6c6f85',
        brightRed: '#d20f39',
        brightGreen: '#40a02b',
        brightYellow: '#df8e1d',
        brightBlue: '#1e66f5',
        brightMagenta: '#ea76cb',
        brightCyan: '#179299',
        brightWhite: '#bcc0cc',
      },
      brand: '#8839ef',
      dim: '#6c6f85',
      link: '#1e66f5',
      matchBackground: '#c9d8f5',
      activeMatchBackground: '#97b7f5',
      matchRuler: '#1e66f5',
    },
  },
  tokyo: {
    label: 'Tokyo Night',
    dark: {
      background: '#1a1b26',
      foreground: '#a9b1d6',
      cursor: '#a9b1d6',
      cursorAccent: '#1a1b26',
      selectionBackground: 'rgba(122, 162, 247, .32)',
      ansi: {
        black: '#32344a',
        red: '#f7768e',
        green: '#9ece6a',
        yellow: '#e0af68',
        blue: '#7aa2f7',
        magenta: '#ad8ee6',
        cyan: '#449dab',
        white: '#787c99',
        brightBlack: '#444b6a',
        brightRed: '#ff7a93',
        brightGreen: '#b9f27c',
        brightYellow: '#ff9e64',
        brightBlue: '#7da6ff',
        brightMagenta: '#bb9af7',
        brightCyan: '#0db9d7',
        brightWhite: '#acb0d0',
      },
      brand: '#bb9af7',
      dim: '#787c99',
      link: '#7aa2f7',
      matchBackground: '#323d5a',
      activeMatchBackground: '#4a5e8e',
      matchRuler: '#7aa2f7',
    },
  },
  nord: {
    label: 'Nord',
    dark: {
      background: '#2e3440',
      foreground: '#d8dee9',
      cursor: '#d8dee9',
      cursorAccent: '#2e3440',
      selectionBackground: 'rgba(129, 161, 193, .32)',
      ansi: {
        black: '#3b4252',
        red: '#bf616a',
        green: '#a3be8c',
        yellow: '#ebcb8b',
        blue: '#81a1c1',
        magenta: '#b48ead',
        cyan: '#88c0d0',
        white: '#e5e9f0',
        brightBlack: '#4c566a',
        brightRed: '#bf616a',
        brightGreen: '#a3be8c',
        brightYellow: '#ebcb8b',
        brightBlue: '#81a1c1',
        brightMagenta: '#b48ead',
        brightCyan: '#8fbcbb',
        brightWhite: '#eceff4',
      },
      brand: '#88c0d0',
      dim: '#a3adc0',
      link: '#81a1c1',
      matchBackground: '#434f60',
      activeMatchBackground: '#586a80',
      matchRuler: '#81a1c1',
    },
  },
  one: {
    label: 'One Dark',
    dark: {
      background: '#282c34',
      foreground: '#abb2bf',
      cursor: '#abb2bf',
      cursorAccent: '#282c34',
      selectionBackground: 'rgba(97, 175, 239, .32)',
      ansi: {
        black: '#1e2127',
        red: '#e06c75',
        green: '#98c379',
        yellow: '#d19a66',
        blue: '#61afef',
        magenta: '#c678dd',
        cyan: '#56b6c2',
        white: '#abb2bf',
        brightBlack: '#5c6370',
        brightRed: '#e06c75',
        brightGreen: '#98c379',
        brightYellow: '#d19a66',
        brightBlue: '#61afef',
        brightMagenta: '#c678dd',
        brightCyan: '#56b6c2',
        brightWhite: '#ffffff',
      },
      brand: '#c678dd',
      dim: '#7f848e',
      link: '#61afef',
      matchBackground: '#364d63',
      activeMatchBackground: '#446e92',
      matchRuler: '#61afef',
    },
  },
  dracula: {
    label: 'Dracula',
    dark: {
      background: '#282a36',
      foreground: '#f8f8f2',
      cursor: '#f8f8f2',
      cursorAccent: '#282a36',
      selectionBackground: 'rgba(139, 233, 253, .32)',
      ansi: {
        // black 用 Dracula 的 current-line #44475a 而非上游 TOML 的 #000000：
        // 纯黑在黑底上等于隐形（ls 目录色、光标下字符都会糊）
        black: '#44475a',
        red: '#ff5555',
        green: '#50fa7b',
        yellow: '#f1fa8c',
        blue: '#bd93f9',
        magenta: '#ff79c6',
        cyan: '#8be9fd',
        white: '#bbbbbb',
        brightBlack: '#555555',
        brightRed: '#ff5555',
        brightGreen: '#50fa7b',
        brightYellow: '#f1fa8c',
        brightBlue: '#caa9fa',
        brightMagenta: '#ff79c6',
        brightCyan: '#8be9fd',
        brightWhite: '#ffffff',
      },
      brand: '#ff79c6',
      dim: '#bbbbbb',
      link: '#8be9fd',
      matchBackground: '#415a68',
      activeMatchBackground: '#5a8a9a',
      matchRuler: '#8be9fd',
    },
  },
}

export const TERM_SCHEME_IDS = Object.keys(TERM_SCHEMES) as TermSchemeId[]

/* ---- 背景颜色（独立于配色方案；FinalShell 同款三分法 + 跟随界面） ---- */

/** FinalShell 深蓝：其 Default 主题的底色，蓝色背景预设的取值 */
export const TERM_BG_BLUE = '#002945'

/** 背景取向：follow 跟随界面主题深浅 / dark 深色 / light 浅色 / blue FinalShell 深蓝 */
export const TERM_BGS = ['follow', 'dark', 'light', 'blue'] as const
export type TermBg = (typeof TERM_BGS)[number]

export function normalizeTermBg(v: string): TermBg {
  return (TERM_BGS as readonly string[]).includes(v) ? (v as TermBg) : 'follow'
}

/**
 * 脏值兜底。历史值是平铺的方案 id（auto/ink/light/fs）：
 * auto→fs、light→ink（取其浅底变体），其余未知值按默认方案。
 */
const LEGACY_SCHEME_IDS: Record<string, TermSchemeId> = { auto: 'fs', light: 'ink' }
export function normalizeTermScheme(v: string): TermSchemeId {
  if (v in TERM_SCHEMES) return v as TermSchemeId
  return LEGACY_SCHEME_IDS[v] ?? 'fs'
}

/** #RRGGBB → xterm truecolor SGR 前景参数（MOTD 逐行着色用） */
export function hexToSgr(hex: string): string {
  const n = parseInt(hex.slice(1), 16)
  return `38;2;${(n >> 16) & 255};${(n >> 8) & 255};${n & 255}`
}

/* ---- 背景图片：随包内置壁纸 + 用户本地图 ---- */

/**
 * 随包内置壁纸：逐张取自 FinalShell 程序包 `finalshell.jar` 的 `resources/images/bg1-5.jpg`，
 * 原图未改（bg1 深蓝渐变是 FinalShell 的默认背景）。文件在 `frontend/public/bg/`，
 * 按 `/bg/<name>` 绝对路径引用，与内置字体同一套（Tauri 协议下可用，vite base 为默认 `/`）。
 * 配置里存的是 `builtin:bg3.jpg` 这样的引用而不是 dataURL：五张图 6MB，
 * 不该被复制进 app_config.json，导出/导入也只需带走一个文件名。
 */
export const TERM_BG_BUILTINS = ['bg1.jpg', 'bg2.jpg', 'bg3.jpg', 'bg4.jpg', 'bg5.jpg']
export const TERM_BG_BUILTIN_PREFIX = 'builtin:'

/** 内置壁纸的存储值 */
export function termBgBuiltinKey(name: string): string {
  return `${TERM_BG_BUILTIN_PREFIX}${name}`
}

/** 设置页选择器用的小图（`public/bg/thumbs/`，240px）：原图最大 4K，
 *  五张一起解码会白占上百 MB 内存，只有真正选中的那张才按原图加载 */
export function termBgBuiltinThumb(name: string): string {
  return `/bg/thumbs/${name}`
}

/** 存储值 → 可直接进 CSS `url()` 的地址：内置走随包路径，本地图已是 dataURL */
export function resolveTermBgImage(value: string): string {
  return value.startsWith(TERM_BG_BUILTIN_PREFIX)
    ? `/bg/${value.slice(TERM_BG_BUILTIN_PREFIX.length)}`
    : value
}

/** 设置页展示名：内置显文件名（与 FinalShell 一致），本地图没有可展示的名字 */
export function termBgImageLabel(value: string): string {
  if (!value) return ''
  return value.startsWith(TERM_BG_BUILTIN_PREFIX)
    ? `${value.slice(TERM_BG_BUILTIN_PREFIX.length)}（内置）`
    : '本地图片'
}

/* ---- 亮度与对比度（开背景图片时用它决定墨色，见 stores/settings.ts 的 termInkBg） ---- */

/** sRGB 单分量（0~255）的线性亮度，WCAG 定义；逐像素采样直接用它，不必拼 hex */
export function srgbChannelLuma(v: number): number {
  const s = v / 255
  return s <= 0.03928 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4
}

/** sRGB 相对亮度（WCAG 定义，0=全黑 1=全白）；接受 #RGB / #RRGGBB */
export function srgbLuminance(hex: string): number {
  const h = hex.replace('#', '')
  const full = h.length === 3 ? h.split('').map(c => c + c).join('') : h
  const chan = (i: number) => srgbChannelLuma(parseInt(full.slice(i, i + 2), 16))
  return 0.2126 * chan(0) + 0.7152 * chan(1) + 0.0722 * chan(2)
}

/** 两个相对亮度之间的 WCAG 对比度（1~21） */
export function contrastRatio(a: number, b: number): number {
  const hi = Math.max(a, b)
  const lo = Math.min(a, b)
  return (hi + 0.05) / (lo + 0.05)
}
