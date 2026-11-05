<script setup lang="ts">
import { computed, nextTick, onMounted, onUnmounted, ref, watch } from 'vue'

/**
 * 修改文件权限对话框（对齐 FinalShell）：三种主体 × 读/写/执行勾选，
 * 目录另有「递归设置子目录」+ 三种作用范围。只表达低 9 位（0777）：
 * setuid/setgid/sticky 不参与，改完列表里的 s/S/t/T 会回落成普通 x/-。
 */
const props = defineProps<{
  visible: boolean
  entry: { name: string; perm: string; isDir: boolean; isSymlink: boolean } | null
  busy?: boolean
}>()
const emit = defineEmits<{
  (e: 'update:visible', v: boolean): void
  (e: 'apply', p: { mode: number; recursive: boolean; scope: PermScope }): void
}>()

type PermScope = 'all' | 'files' | 'dirs'

const GROUPS = [
  { label: '所有者', offset: 0 },
  { label: '组', offset: 3 },
  { label: '其他', offset: 6 },
] as const
const COLS = ['读取', '写入', '执行'] as const
const SCOPES: { v: PermScope; t: string }[] = [
  { v: 'all', t: '应用到文件和目录' },
  { v: 'files', t: '只应用到文件' },
  { v: 'dirs', t: '只应用到目录' },
]

/** 9 位勾选态，顺序同 ls -l：所有者 r/w/x、组 r/w/x、其他 r/w/x */
const bits = ref<boolean[]>(Array(9).fill(false))
const recursive = ref(false)
const scope = ref<PermScope>('all')
/** 打开瞬间的目标权限，用于判定「没改动」 */
const origMode = ref(0)
const okBtn = ref<HTMLButtonElement | null>(null)

/**
 * 列表的 perm 列是 9 字符串（后端 fmt_perm，无类型前缀位）：
 * 's'/'t' 也算执行位，'S'/'T' 是「有高位但无执行位」→ 不勾。
 */
function parsePerm(perm: string): boolean[] {
  const s = perm.padEnd(9, '-').slice(0, 9)
  return [...s].map((c, i) => {
    switch (i % 3) {
      case 0: return c === 'r'
      case 1: return c === 'w'
      default: return c === 'x' || c === 's' || c === 't'
    }
  })
}
function modeOfBits(b: boolean[]): number {
  return b.reduce((acc, on, i) => (on ? acc | (1 << (8 - i)) : acc), 0)
}

/* 打开时按目标当前权限回填，递归态每次从关开始（误勾的代价比漏勾大） */
watch(() => props.visible, v => {
  if (!v) return
  bits.value = parsePerm(props.entry?.perm ?? '')
  origMode.value = modeOfBits(bits.value)
  recursive.value = false
  scope.value = 'all'
  nextTick(() => setTimeout(() => okBtn.value?.focus(), 120))
})

const mode = computed(() => modeOfBits(bits.value))
const octal = computed(() => mode.value.toString(8).padStart(3, '0'))
const rwx = computed(() =>
  bits.value.map((on, i) => (on ? 'rwx'[i % 3] : '-')).join(''),
)
/** 权限位没变且没开递归时，确定按钮无意义，禁掉避免空跑一趟 SFTP 通道 */
const unchanged = computed(() => !props.entry || (mode.value === origMode.value && !recursive.value))

function close() {
  if (props.busy) return
  emit('update:visible', false)
}
function submit() {
  if (!props.entry || props.busy || unchanged.value) return
  emit('apply', {
    mode: mode.value,
    recursive: recursive.value && !!props.entry.isDir,
    scope: scope.value,
  })
}

function onKey(e: KeyboardEvent) {
  if (!props.visible) return
  if (e.key === 'Escape') {
    e.preventDefault()
    close()
  } else if (e.key === 'Enter') {
    e.preventDefault()
    submit()
  }
}
onMounted(() => document.addEventListener('keydown', onKey))
onUnmounted(() => document.removeEventListener('keydown', onKey))
</script>

<template>
  <div class="mask" :class="{ show: visible }" @click.self="close">
    <div class="modal pm-modal" role="dialog" aria-modal="true" aria-labelledby="pmTitle">
      <div class="modal-head">
        <div class="modal-icon">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"
               stroke-linecap="round" stroke-linejoin="round">
            <rect x="4" y="10" width="16" height="11" rx="2" />
            <path d="M8 10V7a4 4 0 0 1 8 0v3" />
          </svg>
        </div>
        <div class="modal-title">
          <h2 id="pmTitle">修改文件权限</h2>
          <p>{{ entry?.name }}</p>
        </div>
        <button class="modal-close" title="关闭" @click="close">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2"
               stroke-linecap="round">
            <line x1="18" y1="6" x2="6" y2="18" />
            <line x1="6" y1="6" x2="18" y2="18" />
          </svg>
        </button>
      </div>

      <div class="pm-body">
        <div v-for="g in GROUPS" :key="g.label" class="pm-box pm-group">
          <span class="pm-group-label">{{ g.label }}</span>
          <label v-for="(c, ci) in COLS" :key="c" class="km-check pm-check">
            <input type="checkbox" v-model="bits[g.offset + ci]" />
            <span>{{ c }}</span>
          </label>
        </div>

        <div class="pm-box pm-recursive">
          <label class="km-check pm-check" :class="{ dim: !entry?.isDir }">
            <input type="checkbox" v-model="recursive" :disabled="!entry?.isDir" />
            <span>递归设置子目录</span>
            <span v-if="!entry?.isDir" class="pm-hint">文件无需递归</span>
          </label>
          <div class="pm-scopes">
            <label v-for="s in SCOPES" :key="s.v" class="km-check pm-check pm-scope"
                   :class="{ dim: !recursive }">
              <input type="radio" name="pm-scope" :value="s.v" v-model="scope" :disabled="!recursive" />
              <span>{{ s.t }}</span>
            </label>
          </div>
        </div>

        <div v-if="entry?.isSymlink" class="pm-note">
          符号链接：SFTP 改的是它指向的目标，链接自身权限恒为 777。
        </div>
      </div>

      <div class="modal-foot">
        <span class="pm-octal"><b>{{ octal }}</b><code>{{ rwx }}</code></span>
        <span class="spacer"></span>
        <button type="button" class="btn ghost" :disabled="busy" @click="close">取消</button>
        <button ref="okBtn" type="button" class="btn primary"
                :class="{ loading: busy }" :disabled="busy || unchanged" @click="submit">
          <span v-if="busy" class="spin"></span>
          确定
        </button>
      </div>
    </div>
  </div>
</template>

<style scoped>
/* 全局 .modal 固定 820×640，本对话框按内容自适应 */
.pm-modal {
  width: 392px;
  height: auto;
  max-height: calc(100vh - 60px);
}
.pm-body {
  padding: 18px 20px 6px;
}
.pm-box {
  position: relative;
  border: 1px solid var(--border);
  border-radius: 8px;
  padding: 11px 14px;
  margin-bottom: 14px;
}
.pm-group {
  display: flex;
  align-items: center;
  gap: 22px;
}
.pm-group-label {
  position: absolute;
  top: -8px;
  left: 10px;
  padding: 0 5px;
  background: var(--panel);
  font-size: 12px;
  color: var(--muted);
  letter-spacing: .2px;
}
.pm-check {
  font-size: 12.5px;
  color: var(--text);
}
.pm-check.dim {
  opacity: .5;
}
.pm-check input {
  flex-shrink: 0;
}
.pm-recursive {
  padding-bottom: 12px;
}
.pm-scopes {
  display: flex;
  flex-direction: column;
  gap: 8px;
  margin: 10px 0 0 22px;
}
.pm-scope {
  gap: 7px;
}
.pm-hint {
  font-size: 11px;
  color: var(--muted-2);
}
.pm-note {
  margin: -4px 2px 0;
  font-size: 11.5px;
  line-height: 1.5;
  color: var(--muted-2);
}
.pm-octal {
  display: flex;
  align-items: baseline;
  gap: 8px;
  font-size: 12px;
  color: var(--muted);
}
.pm-octal b {
  font-family: var(--mono, monospace);
  font-size: 13px;
  color: var(--text-strong);
}
.pm-octal code {
  font-family: var(--mono, monospace);
  letter-spacing: .5px;
}
</style>
