<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue'
import { invoke } from '@tauri-apps/api/core'
import { toast } from '../../composables/useToast'
import { confirmDialog } from '../../composables/useConfirm'

interface MirrorPreset {
  id: string
  label: string
  host: string
}

interface MirrorFile {
  path: string
  kind: string
  urls: string[]
  mirrorlist: boolean
}

interface MirrorBackup {
  id: string
  created: string
  preset: string
  files: number
}

interface MirrorStatus {
  os: string
  arch: string
  distro: string
  distroId: string
  distroMajor: string
  packageManager: string
  managed: boolean
  note?: string
  current?: string
  hosts: string[]
  files: MirrorFile[]
  backups: MirrorBackup[]
  presets: MirrorPreset[]
}

interface MirrorChange {
  message: string
  status: MirrorStatus
}

type RunElevated = <T>(fn: (password?: string) => Promise<T>) => Promise<T>

const props = defineProps<{ sessionId: string; runElevated: RunElevated }>()

const status = ref<MirrorStatus | null>(null)
const loading = ref(false)
const action = ref<'' | 'apply' | 'restore'>('')
const error = ref('')
const probeFirst = ref(true)
const pendingPreset = ref('')

function errText(e: unknown): string {
  if (typeof e === 'string') return e
  if (e instanceof Error) return e.message
  return String(e ?? '未知错误')
}

const currentLabel = computed(() => {
  const id = status.value?.current
  if (!id) return ''
  return status.value?.presets.find(p => p.id === id)?.label ?? id
})

async function load() {
  if (!props.sessionId) return
  loading.value = true
  error.value = ''
  try {
    status.value = await invoke<MirrorStatus>('toolbox_mirror_status', {
      sessionId: props.sessionId,
    })
  } catch (e) {
    error.value = errText(e)
  } finally {
    loading.value = false
  }
}

async function applyPreset(preset: MirrorPreset) {
  if (!status.value || action.value) return
  const ok = await confirmDialog(
    `将把远端软件源切换到「${preset.label}」（${preset.host}），涉及 ${status.value.files.length} 个源文件。\n` +
      '修改前会自动备份原配置，可随时还原。' +
      (probeFirst.value ? '\n切换前会先探测镜像站是否可达，不可达则不做任何修改。' : ''),
    '切换软件源',
    { confirmText: '切换', danger: false },
  )
  if (!ok) return
  action.value = 'apply'
  pendingPreset.value = preset.id
  error.value = ''
  try {
    const res = await props.runElevated(pw =>
      invoke<MirrorChange>('toolbox_mirror_apply', {
        sessionId: props.sessionId,
        preset: preset.id,
        probe: probeFirst.value,
        sudoPassword: pw,
      }),
    )
    status.value = res.status
    toast(res.message)
  } catch (e) {
    error.value = errText(e)
  } finally {
    action.value = ''
    pendingPreset.value = ''
  }
}

async function restore(backup: MirrorBackup) {
  if (action.value) return
  const presetLabel = backup.preset === 'restore'
    ? '还原前的现场'
    : status.value?.presets.find(p => p.id === backup.preset)?.label ?? backup.preset
  const ok = await confirmDialog(
    `将用备份 ${backup.id}（${backup.created}，${presetLabel}，${backup.files} 个文件）覆盖当前软件源配置。\n` +
      '还原前会把当前配置再备份一份，操作本身也能回退。',
    '还原软件源',
    { confirmText: '还原' },
  )
  if (!ok) return
  action.value = 'restore'
  error.value = ''
  try {
    const res = await props.runElevated(pw =>
      invoke<MirrorChange>('toolbox_mirror_restore', {
        sessionId: props.sessionId,
        backup: backup.id,
        sudoPassword: pw,
      }),
    )
    status.value = res.status
    toast(res.message)
  } catch (e) {
    error.value = errText(e)
  } finally {
    action.value = ''
  }
}

const busyLabel = computed(() => {
  if (action.value === 'apply') {
    const p = status.value?.presets.find(x => x.id === pendingPreset.value)
    return `正在切换到${p?.label ?? '镜像站'}…`
  }
  if (action.value === 'restore') return '正在还原…'
  return ''
})

watch(() => props.sessionId, () => {
  status.value = null
  error.value = ''
  void load()
})

onMounted(() => {
  void load()
})
</script>

<template>
  <div class="mr-root">
    <div v-if="error" class="mr-error">
      <span>{{ error }}</span>
      <button class="mr-link" @click="load()">重试</button>
    </div>

    <section class="mr-card">
      <div class="mr-head">
        <h3>软件源</h3>
        <span v-if="status" class="mr-pill">{{ status.packageManager }}</span>
        <span v-if="currentLabel" class="mr-pill on">{{ currentLabel }}</span>
        <span v-else-if="status?.managed" class="mr-pill warn">未命中预设镜像</span>
        <span class="spacer"></span>
        <span v-if="busyLabel" class="mr-busy">{{ busyLabel }}</span>
        <button class="mr-btn" :disabled="loading || !!action" @click="load()">刷新</button>
      </div>
      <div v-if="status" class="mr-meta">
        <span>{{ status.distro || status.os }}</span>
        <span v-if="status.arch">{{ status.arch }}</span>
        <span v-if="status.files.length">{{ status.files.length }} 个源文件</span>
        <span v-if="status.backups.length">{{ status.backups.length }} 个备份</span>
      </div>
      <p v-if="status?.note" class="mr-note">{{ status.note }}</p>
      <p v-if="!status && !loading && !error" class="mr-empty">尚未读取</p>
      <p v-if="loading && !status" class="mr-empty">正在读取软件源…</p>
    </section>

    <section v-if="status?.managed" class="mr-card">
      <div class="mr-head">
        <h3>切换镜像站</h3>
        <label class="mr-check">
          <input v-model="probeFirst" type="checkbox" :disabled="!!action" />
          切换前探测镜像可达
        </label>
      </div>
      <div class="mr-presets">
        <button
          v-for="p in status.presets" :key="p.id"
          class="mr-preset" :class="{ active: p.id === status.current }"
          :disabled="!!action || p.id === status.current"
          @click="applyPreset(p)"
        >
          <span class="mr-preset-label">{{ p.label }}</span>
          <span class="mr-preset-host">{{ p.host }}</span>
          <span v-if="p.id === status.current" class="mr-preset-tag">当前</span>
        </button>
      </div>
      <p class="mr-hint">
        只替换软件源地址中的主机名（保留发行版路径与协议）；CentOS 7 的 mirrorlist 与 EPEL metalink
        会改写为镜像站对应的 baseurl。修改前自动备份到
        <code>/etc/rhost/mirror-backup/</code>。
      </p>
    </section>

    <section v-if="status" class="mr-card">
      <div class="mr-head">
        <h3>源文件</h3>
      </div>
      <div v-if="status.files.length" class="mr-files">
        <div v-for="f in status.files" :key="f.path" class="mr-file">
          <div class="mr-file-head">
            <span class="mr-file-path" :title="f.path">{{ f.path }}</span>
            <span class="mr-file-kind">{{ f.kind }}</span>
            <span v-if="f.mirrorlist" class="mr-file-tag">含 mirrorlist</span>
          </div>
          <div v-if="f.urls.length" class="mr-file-urls">
            <span v-for="u in f.urls" :key="u" class="mr-chip" :title="u">{{ u }}</span>
          </div>
          <div v-else class="mr-file-urls">
            <span class="mr-hint">未识别到直接的 HTTP(S) 源地址</span>
          </div>
        </div>
      </div>
      <p v-else class="mr-empty">未发现软件源文件</p>
    </section>

    <section v-if="status" class="mr-card">
      <div class="mr-head">
        <h3>备份与还原</h3>
        <span v-if="status.backups.length" class="mr-pill">{{ status.backups.length }}</span>
      </div>
      <div v-if="status.backups.length" class="mr-backups">
        <div v-for="b in status.backups" :key="b.id" class="mr-backup">
          <div class="mr-backup-info">
            <span class="mr-backup-id">{{ b.id }}</span>
            <span class="mr-backup-meta">{{ b.created || '时间未知' }} · {{ b.files }} 个文件</span>
          </div>
          <button class="mr-btn" :disabled="!!action" @click="restore(b)">还原</button>
        </div>
      </div>
      <p v-else class="mr-empty">还没有备份；首次切换镜像站时会自动创建</p>
    </section>
  </div>
</template>

<style scoped>
.mr-root {
  flex: 1;
  min-height: 0;
  display: flex;
  flex-direction: column;
  gap: 10px;
  overflow: auto;
}
.mr-card {
  border: 1px solid var(--border);
  border-radius: 9px;
  background: var(--panel-2);
  padding: 12px 14px;
  display: flex;
  flex-direction: column;
  gap: 9px;
  flex-shrink: 0;
}
.mr-head {
  display: flex;
  align-items: center;
  gap: 8px;
  min-width: 0;
}
.mr-head h3 {
  margin: 0;
  font-size: 12.5px;
  font-weight: 600;
  color: var(--text);
  white-space: nowrap;
}
.mr-pill {
  padding: 2px 8px;
  border-radius: 999px;
  border: 1px solid var(--border);
  font-size: 11px;
  color: var(--muted);
  white-space: nowrap;
}
.mr-pill.on {
  border-color: color-mix(in srgb, var(--green) 45%, transparent);
  color: var(--green);
}
.mr-pill.warn {
  border-color: color-mix(in srgb, var(--yellow) 50%, transparent);
  color: var(--yellow);
}
.mr-meta {
  display: flex;
  flex-wrap: wrap;
  gap: 6px 16px;
  font-size: 11px;
  color: var(--muted);
}
.mr-note {
  margin: 0;
  font-size: 11.5px;
  color: var(--muted);
  line-height: 1.6;
}
.mr-check {
  display: inline-flex;
  align-items: center;
  gap: 5px;
  font-size: 11px;
  color: var(--muted);
  cursor: pointer;
  white-space: nowrap;
}
.mr-check input { accent-color: var(--blue); }
.mr-presets {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(180px, 1fr));
  gap: 8px;
}
.mr-preset {
  position: relative;
  display: flex;
  flex-direction: column;
  gap: 2px;
  padding: 8px 10px;
  border: 1px solid var(--border);
  border-radius: 8px;
  background: var(--panel-2);
  font: inherit;
  text-align: left;
  cursor: pointer;
  min-width: 0;
}
.mr-preset:hover:not(:disabled) { border-color: var(--border-strong); background: var(--hover); }
.mr-preset:disabled { cursor: default; opacity: .85; }
.mr-preset.active {
  border-color: color-mix(in srgb, var(--green) 50%, transparent);
}
.mr-preset-label {
  font-size: 12px;
  color: var(--text);
}
.mr-preset-host {
  font-family: var(--mono, monospace);
  font-size: 10.5px;
  color: var(--muted);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.mr-preset-tag {
  position: absolute;
  top: 6px;
  right: 8px;
  font-size: 10px;
  color: var(--green);
}
.mr-files {
  display: flex;
  flex-direction: column;
  gap: 8px;
}
.mr-file {
  border: 1px solid var(--border);
  border-radius: 8px;
  padding: 7px 9px;
  display: flex;
  flex-direction: column;
  gap: 5px;
  min-width: 0;
}
.mr-file-head {
  display: flex;
  align-items: center;
  gap: 8px;
  min-width: 0;
}
.mr-file-path {
  font-family: var(--mono, monospace);
  font-size: 11px;
  color: var(--text);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.mr-file-kind {
  padding: 1px 6px;
  border-radius: 999px;
  border: 1px solid var(--border);
  font-size: 10px;
  color: var(--muted);
}
.mr-file-tag {
  font-size: 10px;
  color: var(--yellow);
  white-space: nowrap;
}
.mr-file-urls {
  display: flex;
  flex-wrap: wrap;
  gap: 5px;
}
.mr-chip {
  padding: 2px 7px;
  border-radius: 6px;
  border: 1px solid var(--border);
  font-family: var(--mono, monospace);
  font-size: 10.5px;
  color: var(--text);
  max-width: 100%;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.mr-backups {
  display: flex;
  flex-direction: column;
  gap: 6px;
}
.mr-backup {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 6px 9px;
  border: 1px solid var(--border);
  border-radius: 8px;
  min-width: 0;
}
.mr-backup-info {
  display: flex;
  flex-direction: column;
  gap: 2px;
  min-width: 0;
  flex: 1;
}
.mr-backup-id {
  font-family: var(--mono, monospace);
  font-size: 11px;
  color: var(--text);
}
.mr-backup-meta {
  font-size: 10.5px;
  color: var(--muted);
}
.mr-btn {
  padding: 6px 12px;
  border: 1px solid var(--border);
  border-radius: 7px;
  background: var(--panel-2);
  font: inherit;
  font-size: 11.5px;
  color: var(--text);
  cursor: pointer;
  white-space: nowrap;
}
.mr-btn:hover:not(:disabled) { border-color: var(--border-strong); background: var(--hover); }
.mr-btn:disabled { opacity: .5; cursor: default; }
.mr-busy {
  font-size: 11px;
  color: var(--muted);
}
.mr-hint {
  margin: 0;
  font-size: 11px;
  color: var(--muted-2);
  line-height: 1.6;
}
.mr-hint code {
  font-family: var(--mono, monospace);
  font-size: 10.5px;
}
.mr-empty {
  margin: 0;
  padding: 8px 0;
  text-align: center;
  font-size: 12px;
  color: var(--muted-2);
}
.mr-error {
  display: flex;
  align-items: flex-start;
  gap: 10px;
  padding: 8px 10px;
  border: 1px solid color-mix(in srgb, var(--red) 45%, transparent);
  border-radius: 8px;
  background: color-mix(in srgb, var(--red) 10%, transparent);
  font-size: 11.5px;
  color: var(--red);
  word-break: break-all;
  flex-shrink: 0;
}
.mr-link {
  border: 0;
  background: none;
  padding: 0;
  font: inherit;
  font-size: 11.5px;
  color: var(--blue);
  cursor: pointer;
  white-space: nowrap;
}
.spacer { flex: 1; }
</style>
