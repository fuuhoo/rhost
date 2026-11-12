<script setup lang="ts">
import { computed } from 'vue'
import HostToolbar from '../../components/HostToolbar.vue'
import HostList from '../../components/HostList.vue'
import GroupList from '../../components/GroupList.vue'
import { filter, filterInfo, hosts, styleLabel } from '../../stores/hosts'
import { groupedView } from '../../stores/hostDisplay'
import { groupStats } from '../../stores/groups'

const onlineTotal = computed(() => hosts.value.filter(h => h.status === 'online').length)
const keyword = computed(() => filter.value.trim())
</script>

<template>
  <HostToolbar />

  <div class="divider"></div>

  <div v-if="groupedView" class="list-meta">
    共 <b>{{ groupStats.length }}</b> 个分组
    <span class="sep">·</span>
    <span>覆盖 <b>{{ hosts.length }}</b> 台主机（在线 <b>{{ onlineTotal }}</b>）</span>
    <span class="sep">·</span>
    <span>{{ keyword ? `搜索「${keyword}」` : '无筛选' }}</span>
  </div>
  <div v-else class="list-meta">
    共 <b>{{ hosts.length }}</b> 台主机
    <span class="sep">·</span>
    <span>当前显示：</span>
    <b>{{ styleLabel }}</b>
    <span class="sep">·</span>
    <span>{{ filterInfo }}</span>
  </div>

  <GroupList v-if="groupedView" />
  <HostList v-else />
</template>
