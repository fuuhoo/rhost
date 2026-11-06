<script setup lang="ts">
// 文件列表行图标：文件夹=金色双层，文件=带折角的立体文档。
// 文档底色继承 .row-icon 的 fill（深浅主题各自取值），立体感用中性黑/白叠加层做明暗，
// 因此同一组渐变两套主题通用；轮廓描边保证浅底上仍读得出边缘。
defineProps<{ dir?: boolean }>()
</script>

<template>
  <svg class="row-icon" viewBox="0 0 24 24">
    <defs>
      <linearGradient id="rk-folder-back" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#E8B34A"/><stop offset="1" stop-color="#C98F22"/></linearGradient>
      <linearGradient id="rk-folder-front" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#FFE9A8"/><stop offset=".55" stop-color="#F8CB4C"/><stop offset="1" stop-color="#EEA82E"/></linearGradient>
      <linearGradient id="rk-doc-body" x1=".1" y1="0" x2=".55" y2="1">
        <stop offset="0" stop-color="#fff" stop-opacity=".34"/>
        <stop offset=".45" stop-color="#fff" stop-opacity=".04"/>
        <stop offset="1" stop-color="#000" stop-opacity=".26"/>
      </linearGradient>
      <linearGradient id="rk-doc-fold" x1="1" y1="0" x2="0" y2="1">
        <stop offset="0" stop-color="#fff" stop-opacity=".55"/>
        <stop offset="1" stop-color="#000" stop-opacity=".1"/>
      </linearGradient>
    </defs>
    <g v-if="dir" stroke="#9C6F14" stroke-width=".7" stroke-linejoin="round">
      <path d="M2.8 6.4c0-1 .8-1.7 1.8-1.7h4.5l2.2 2.6h9.5c1 0 1.8.8 1.8 1.8v1.6H2.8z" fill="url(#rk-folder-back)"/>
      <path d="M3.4 9.6h17.2l-1.1 8.8c-.1 1-.9 1.7-1.9 1.7H6.4c-1 0-1.8-.7-1.9-1.7z" fill="url(#rk-folder-front)"/>
    </g>
    <g v-else>
      <path d="M14 3H6a2 2 0 00-2 2v14a2 2 0 002 2h12a2 2 0 002-2V9z"/>
      <path d="M14 3H6a2 2 0 00-2 2v14a2 2 0 002 2h12a2 2 0 002-2V9z" fill="url(#rk-doc-body)"/>
      <path d="M14 3v6h6" fill="url(#rk-doc-fold)"/>
      <path d="M14 3v6h6" fill="none" stroke="#000" stroke-opacity=".24" stroke-width=".7" stroke-linejoin="round"/>
      <path d="M14 3H6a2 2 0 00-2 2v14a2 2 0 002 2h12a2 2 0 002-2V9z" fill="none" stroke="#000" stroke-opacity=".24" stroke-width=".7"/>
    </g>
  </svg>
</template>
