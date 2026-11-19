<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue'
import { invoke } from '@tauri-apps/api/core'
import { toast } from '../../composables/useToast'

interface NetAddr {
  family: string
  address: string
  prefix?: number
  scope?: string
}

interface NetIface {
  name: string
  state: string
  mtu?: number
  mac?: string
  addrs: NetAddr[]
  default: boolean
}

interface NetRoute {
  family: string
  destination: string
  gateway?: string
  device?: string
  protocol?: string
  scope?: string
  metric?: number
  src?: string
  default: boolean
}

interface NetDns {
  servers: string[]
  search: string[]
  options: string[]
}

interface NetworkInfo {
  hostname?: string
  os?: string
  tool: string
  interfaces: NetIface[]
  routes: NetRoute[]
  dns: NetDns
  note?: string
}

const props = defineProps<{ sessionId: string }>()

const info = ref<NetworkInfo | null>(null)
const loading = ref(false)
const error = ref('')
const query = ref('')

function errText(e: unknown): string {
  if (typeof e === 'string') return e
  if (e instanceof Error) return e.message
  return String(e ?? '未知错误')
}

async function load() {
  if (!props.sessionId) return
  loading.value = true
  error.value = ''
  try {
    info.value = await invoke<NetworkInfo>('toolbox_network_info', {
      sessionId: props.sessionId,
    })
  } catch (e) {
    error.value = errText(e)
  } finally {
    loading.value = false
  }
}

const toolLabel = computed(() => {
  switch (info.value?.tool) {
    case 'ip': return 'iproute2'
    case 'legacy': return 'ifconfig'
    default: return '未检测到'
  }
})

const gatewayLabel = computed(() => {
  const defaults = info.value?.routes.filter(r => r.default) ?? []
  if (!defaults.length) return '—'
  return defaults
    .map(r => r.gateway ?? (r.device ? `${r.device} 直连` : '直连'))
    .join('，')
})

const q = computed(() => query.value.trim().toLowerCase())

function hit(...parts: (string | number | undefined | null)[]): boolean {
  if (!q.value) return true
  return parts.some(p => p !== undefined && p !== null && String(p).toLowerCase().includes(q.value))
}

function addrHit(a: NetAddr): boolean {
  return hit(a.family, a.address, a.prefix, a.scope)
}

const ifaces = computed(() => {
  const list = info.value?.interfaces ?? []
  if (!q.value) return list
  return list.filter(i => hit(i.name, i.state, i.mtu, i.mac) || i.addrs.some(addrHit))
})

const routes = computed(() => {
  const list = info.value?.routes ?? []
  if (!q.value) return list
  return list.filter(r => hit(r.family, r.destination, r.gateway, r.device, r.protocol, r.scope, r.metric, r.src))
})

const dnsServers = computed(() => {
  const list = info.value?.dns.servers ?? []
  return q.value ? list.filter(s => hit(s)) : list
})

const dnsSearch = computed(() => {
  const list = info.value?.dns.search ?? []
  return q.value ? list.filter(s => hit(s)) : list
})

const dnsOptions = computed(() => {
  const list = info.value?.dns.options ?? []
  return q.value ? list.filter(s => hit(s)) : list
})

const totalLabel = computed(() => {
  const i = info.value?.interfaces.length ?? 0
  const r = info.value?.routes.length ?? 0
  if (!q.value) return `${i} 个网卡 · ${r} 条路由`
  return `命中 ${ifaces.value.length}/${i} 网卡 · ${routes.value.length}/${r} 路由`
})

const noHit = computed(() => {
  if (!q.value) return false
  return !ifaces.value.length && !routes.value.length && !dnsServers.value.length
    && !dnsSearch.value.length && !dnsOptions.value.length
})

function addrText(a: NetAddr): string {
  return a.prefix === undefined ? a.address : `${a.address}/${a.prefix}`
}

async function copyAddr(a: NetAddr) {
  const text = addrText(a)
  try {
    await navigator.clipboard.writeText(text)
    toast(`已复制 ${text}`)
  } catch {
    toast('复制失败', 'err')
  }
}

function stateLabel(state: string): string {
  if (state === 'UP') return '已启用'
  if (state === 'DOWN') return '未启用'
  return '未知'
}

watch(() => props.sessionId, () => {
  info.value = null
  query.value = ''
  error.value = ''
  void load()
})

onMounted(() => { void load() })
</script>

<template>
  <div class="nw-root">
    <div class="nw-bar">
      <input
        v-model="query" class="nw-input nw-search"
        placeholder="搜索网卡 / IP / MAC / 路由 / DNS…"
        autocomplete="off" spellcheck="false"
      />
      <span class="spacer"></span>
      <span v-if="info" class="nw-count">{{ totalLabel }}</span>
      <span class="nw-pill" :title="info?.os ?? ''">{{ toolLabel }}</span>
      <button class="nw-btn" :disabled="loading" @click="load()">刷新</button>
    </div>

    <div v-if="error" class="nw-error">
      <span>{{ error }}</span>
      <button class="nw-link" @click="load()">重试</button>
    </div>

    <div v-if="info" class="nw-card">
      <div class="nw-summary">
        <div class="nw-sum-item">
          <span class="nw-sum-label">主机名</span>
          <span class="nw-sum-value">{{ info.hostname || '—' }}</span>
        </div>
        <div class="nw-sum-item">
          <span class="nw-sum-label">系统</span>
          <span class="nw-sum-value">{{ info.os || '—' }}</span>
        </div>
        <div class="nw-sum-item">
          <span class="nw-sum-label">默认网关</span>
          <span class="nw-sum-value">{{ gatewayLabel }}</span>
        </div>
        <div class="nw-sum-item">
          <span class="nw-sum-label">DNS</span>
          <span class="nw-sum-value">{{ info.dns.servers.length ? info.dns.servers.join('，') : '—' }}</span>
        </div>
      </div>
      <p v-if="info.note" class="nw-note">{{ info.note }}</p>
    </div>

    <div v-if="info && noHit" class="nw-empty">没有匹配「{{ query.trim() }}」的网络信息</div>

    <template v-if="info && !noHit">
      <section class="nw-card">
        <div class="nw-head">
          <h3>网卡</h3>
          <span class="nw-dim">{{ ifaces.length }} 个</span>
        </div>
        <div v-if="!ifaces.length" class="nw-empty inner">{{ query.trim() ? '没有匹配的网卡' : '未检测到网卡' }}</div>
        <div v-for="i in ifaces" :key="i.name" class="nw-iface">
          <div class="nw-iface-head">
            <span class="nw-name">{{ i.name }}</span>
            <span class="nw-state" :class="i.state === 'UP' ? 'on' : i.state === 'DOWN' ? 'off' : ''">
              {{ stateLabel(i.state) }}
            </span>
            <span v-if="i.default" class="nw-badge">默认路由</span>
            <span class="spacer"></span>
            <span class="nw-meta">
              <template v-if="i.mtu">MTU {{ i.mtu }}</template>
              <template v-if="i.mtu && i.mac"> · </template>
              <template v-if="i.mac">{{ i.mac }}</template>
            </span>
          </div>
          <div v-if="i.addrs.length" class="nw-addrs">
            <button
              v-for="a in i.addrs" :key="a.family + a.address"
              class="nw-addr" :title="`点击复制 ${addrText(a)}`" @click="copyAddr(a)"
            >
              <span class="nw-fam" :class="a.family === 'ipv4' ? 'v4' : 'v6'">
                {{ a.family === 'ipv4' ? 'IPv4' : 'IPv6' }}
              </span>
              <span>{{ addrText(a) }}</span>
              <span v-if="a.scope" class="nw-scope">{{ a.scope }}</span>
            </button>
          </div>
          <div v-else class="nw-dim">无地址</div>
        </div>
      </section>

      <section class="nw-card">
        <div class="nw-head">
          <h3>路由</h3>
          <span class="nw-dim">{{ routes.length }} 条</span>
        </div>
        <div v-if="!routes.length" class="nw-empty inner">{{ query.trim() ? '没有匹配的路由' : '未检测到路由' }}</div>
        <div v-else class="nw-table">
          <div class="nw-tr nw-th">
            <span>族</span>
            <span>目标</span>
            <span>网关</span>
            <span>网卡</span>
            <span>协议</span>
            <span>指标</span>
            <span>来源</span>
          </div>
          <div v-for="(r, n) in routes" :key="n" class="nw-tr" :class="{ def: r.default }">
            <span><span class="nw-fam" :class="r.family === 'ipv4' ? 'v4' : 'v6'">{{ r.family === 'ipv4' ? 'v4' : 'v6' }}</span></span>
            <span class="nw-dest" :title="r.scope ? `scope ${r.scope}` : ''">{{ r.destination }}</span>
            <span>{{ r.gateway || '—' }}</span>
            <span>{{ r.device || '—' }}</span>
            <span>{{ r.protocol || '—' }}</span>
            <span>{{ r.metric ?? '—' }}</span>
            <span>{{ r.src || '—' }}</span>
          </div>
        </div>
      </section>

      <section class="nw-card">
        <div class="nw-head">
          <h3>DNS</h3>
          <span class="nw-dim">/etc/resolv.conf</span>
        </div>
        <div v-if="!dnsServers.length && !dnsSearch.length && !dnsOptions.length" class="nw-empty inner">
          {{ query.trim() ? '没有匹配的 DNS 配置' : '未检测到 DNS 配置' }}
        </div>
        <template v-else>
          <div v-if="dnsServers.length" class="nw-dns-row">
            <span class="nw-sum-label">服务器</span>
            <div class="nw-chips">
              <span v-for="s in dnsServers" :key="s" class="nw-chip">{{ s }}</span>
            </div>
          </div>
          <div v-if="dnsSearch.length" class="nw-dns-row">
            <span class="nw-sum-label">搜索域</span>
            <div class="nw-chips">
              <span v-for="s in dnsSearch" :key="s" class="nw-chip">{{ s }}</span>
            </div>
          </div>
          <div v-if="dnsOptions.length" class="nw-dns-row">
            <span class="nw-sum-label">选项</span>
            <div class="nw-chips">
              <span v-for="s in dnsOptions" :key="s" class="nw-chip">{{ s }}</span>
            </div>
          </div>
        </template>
      </section>
    </template>

    <p v-if="!info && !loading && !error" class="nw-empty">尚未读取</p>
    <p v-if="loading && !info" class="nw-empty">正在读取网络信息…</p>
  </div>
</template>

<style scoped>
.nw-root {
  flex: 1;
  min-height: 0;
  display: flex;
  flex-direction: column;
  gap: 10px;
  overflow: auto;
}
.nw-bar {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-shrink: 0;
  min-width: 0;
}
.nw-input {
  padding: 6px 9px;
  border: 1px solid var(--border);
  border-radius: 7px;
  background: var(--panel-2);
  font: inherit;
  font-size: 11.5px;
  color: var(--text);
  min-width: 0;
}
.nw-input:focus { outline: none; border-color: var(--border-strong); }
.nw-search { flex: 1; max-width: 360px; }
.nw-count {
  font-size: 11px;
  color: var(--muted);
  white-space: nowrap;
}
.nw-pill {
  padding: 2px 8px;
  border: 1px solid var(--border);
  border-radius: 999px;
  font-size: 10.5px;
  color: var(--muted);
  white-space: nowrap;
}
.nw-btn {
  padding: 5px 12px;
  border: 1px solid var(--border);
  border-radius: 7px;
  background: var(--panel-2);
  color: var(--text);
  font: inherit;
  font-size: 11.5px;
  cursor: pointer;
  white-space: nowrap;
}
.nw-btn:hover:not(:disabled) { background: var(--hover); }
.nw-btn:disabled { opacity: .5; cursor: default; }
.nw-link {
  background: none;
  border: none;
  padding: 0;
  color: var(--green);
  cursor: pointer;
  font: inherit;
}
.nw-error {
  display: flex;
  gap: 8px;
  align-items: center;
  padding: 8px 10px;
  border: 1px solid color-mix(in srgb, var(--red) 40%, transparent);
  border-radius: 8px;
  color: var(--red);
  font-size: 11.5px;
  flex-shrink: 0;
}
.nw-card {
  border: 1px solid var(--border);
  border-radius: 9px;
  background: var(--panel-2);
  padding: 12px 14px;
  flex-shrink: 0;
}
.nw-summary {
  display: grid;
  grid-template-columns: repeat(2, minmax(0, 1fr));
  gap: 8px 20px;
}
.nw-sum-item {
  display: flex;
  gap: 8px;
  min-width: 0;
  font-size: 11.5px;
}
.nw-sum-label {
  color: var(--muted);
  flex-shrink: 0;
}
.nw-sum-value {
  color: var(--text);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.nw-note {
  margin: 8px 0 0;
  font-size: 11px;
  color: var(--muted);
}
.nw-head {
  display: flex;
  align-items: baseline;
  gap: 8px;
  margin-bottom: 8px;
}
.nw-head h3 {
  margin: 0;
  font-size: 12.5px;
  font-weight: 600;
  color: var(--text);
}
.nw-dim { font-size: 11px; color: var(--muted); }
.nw-empty {
  padding: 18px 0;
  text-align: center;
  font-size: 11.5px;
  color: var(--muted);
}
.nw-empty.inner { padding: 10px 0; }
.nw-iface {
  padding: 8px 0;
  border-top: 1px solid var(--border-soft);
}
.nw-iface:first-of-type { border-top: none; padding-top: 0; }
.nw-iface-head {
  display: flex;
  align-items: center;
  gap: 8px;
  min-width: 0;
}
.nw-name {
  font-family: var(--mono, monospace);
  font-size: 12px;
  color: var(--text);
}
.nw-state {
  padding: 1px 7px;
  border-radius: 999px;
  font-size: 10px;
  border: 1px solid var(--border);
  color: var(--muted);
}
.nw-state.on { color: var(--green); border-color: color-mix(in srgb, var(--green) 45%, transparent); }
.nw-state.off { color: var(--red); border-color: color-mix(in srgb, var(--red) 40%, transparent); }
.nw-badge {
  padding: 1px 7px;
  border-radius: 999px;
  font-size: 10px;
  color: var(--cyan);
  border: 1px solid color-mix(in srgb, var(--cyan) 40%, transparent);
}
.nw-meta {
  font-size: 10.5px;
  color: var(--muted);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.nw-addrs {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
  margin-top: 6px;
}
.nw-addr {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  padding: 2px 8px;
  border: 1px solid var(--border);
  border-radius: 6px;
  background: var(--panel);
  color: var(--text);
  font-family: var(--mono, monospace);
  font-size: 11px;
  cursor: pointer;
}
.nw-addr:hover { border-color: var(--border-strong); }
.nw-fam {
  font-size: 9.5px;
  padding: 0 4px;
  border-radius: 4px;
  font-family: inherit;
}
.nw-fam.v4 { color: var(--blue); border: 1px solid color-mix(in srgb, var(--blue) 40%, transparent); }
.nw-fam.v6 { color: var(--purple); border: 1px solid color-mix(in srgb, var(--purple) 40%, transparent); }
.nw-scope { font-size: 9.5px; color: var(--muted); font-family: inherit; }
.nw-table { display: flex; flex-direction: column; }
.nw-tr {
  display: grid;
  grid-template-columns: 36px minmax(140px, 1.4fr) minmax(110px, 1.1fr) minmax(90px, .9fr) minmax(60px, .6fr) 56px minmax(100px, 1fr);
  gap: 10px;
  padding: 6px 0;
  border-top: 1px solid var(--border-soft);
  font-size: 11px;
  color: var(--text);
  align-items: center;
}
.nw-tr > span {
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.nw-th {
  border-top: none;
  padding-top: 0;
  color: var(--muted);
  font-size: 10.5px;
}
.nw-tr.def .nw-dest { color: var(--cyan); }
.nw-dest { font-family: var(--mono, monospace); }
.nw-dns-row {
  display: flex;
  align-items: flex-start;
  gap: 10px;
  padding: 3px 0;
  font-size: 11.5px;
}
.nw-dns-row .nw-sum-label { padding-top: 2px; }
.nw-chips { display: flex; flex-wrap: wrap; gap: 6px; }
.nw-chip {
  padding: 1px 8px;
  border: 1px solid var(--border);
  border-radius: 6px;
  background: var(--panel);
  font-family: var(--mono, monospace);
  font-size: 11px;
  color: var(--text);
}
</style>
