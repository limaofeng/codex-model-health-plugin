<script setup lang="ts">
import { reactive } from 'vue'
import { ZButton, ZIconButton } from '@codex-proxy/ui'
import { Copy, RefreshCw, GitBranch } from '@lucide/vue'
import { api } from './api'
import type { Method, UpstreamCheck, UpstreamSource } from './api'
import { formatTime } from './presentation'

defineProps<{ sources: UpstreamSource[] }>()
const results = reactive<Partial<Record<Method, UpstreamCheck>>>({})
const busy = reactive<Partial<Record<Method, boolean>>>({})
const errors = reactive<Partial<Record<Method, string>>>({})
const copies = reactive<Record<string, string>>({})

async function check(source: UpstreamSource) {
  if (busy[source.method]) return
  busy[source.method] = true
  delete errors[source.method]
  delete results[source.method]
  delete copies[`diff-${source.method}`]
  try { results[source.method] = (await api<{check: UpstreamCheck}>('upstreams/check', {method: source.method})).check }
  catch (cause) { errors[source.method] = cause instanceof Error ? cause.message : String(cause) }
  finally { busy[source.method] = false }
}
function status(result: UpstreamCheck) {
  switch (result.status) {
    case 'identical': return '与内置提交一致'
    case 'ahead': return `上游有 ${result.ahead_by} 个新提交`
    case 'behind': return `内置提交领先上游 ${result.behind_by} 个提交`
    case 'diverged': return '提交历史已分叉，需人工核对'
  }
}
function selectAddress(event: Event) { (event.target as HTMLInputElement).select() }
function copyAddress(id: string) {
  const input = document.getElementById(id) as HTMLInputElement
  input.focus(); input.select()
  // 宿主 iframe 不开放新窗口或异步剪贴板；保留原生选区供浏览器手动复制。
  try { copies[id] = document.execCommand('copy') ? '已复制' : '地址已选中，请手动复制' }
  catch { copies[id] = '地址已选中，请手动复制' }
}
</script>

<template>
  <section class="source-updates" aria-labelledby="sources-heading">
    <div class="source-heading"><h3 id="sources-heading"><GitBranch class="size-4" />项目来源</h3><span>手动升级</span></div>
    <div v-for="source in sources" :key="source.method" class="source-entry">
      <div class="source-title">
        <div><strong>{{ source.label }}</strong><span class="source-revision" :title="source.bundled_commit">内置 {{ source.bundled_commit.slice(0,7) }}</span></div>
        <ZButton size="small" variant="text" :loading="busy[source.method]" :aria-label="`检查 ${source.label} 更新`" @click="check(source)"><template #icon><RefreshCw class="size-3.5" /></template>{{ busy[source.method] ? '检查中' : '检查更新' }}</ZButton>
      </div>
      <div class="source-address">
        <input :id="`source-${source.method}`" :aria-label="`${source.label} GitHub 地址`" :value="source.url" :title="source.url" readonly spellcheck="false" @focus="selectAddress" @click="selectAddress">
        <ZIconButton size="small" :label="`复制 ${source.label} 地址`" @click="copyAddress(`source-${source.method}`)"><Copy class="size-3.5" /></ZIconButton>
      </div>
      <span v-if="copies[`source-${source.method}`]" role="status" class="source-copy-note">{{ copies[`source-${source.method}`] }}</span>
      <div v-if="errors[source.method]" role="alert" class="source-check-error">更新状态未确认：{{ errors[source.method] }}</div>
      <div v-else-if="results[source.method]" role="status" class="source-result" :class="`source-result-${results[source.method]!.status}`">
        <strong>{{ status(results[source.method]!) }}</strong>
        <span class="source-checked">{{ formatTime(results[source.method]!.checked_at) }} 检查 · 上游 {{ results[source.method]!.latest_commit.slice(0,7) }}</span>
        <div v-if="results[source.method]!.status !== 'identical'" class="source-address">
          <input :id="`diff-${source.method}`" :aria-label="`${source.label} 提交差异地址`" :value="results[source.method]!.compare_url" :title="results[source.method]!.compare_url" readonly spellcheck="false" @focus="selectAddress" @click="selectAddress">
          <ZIconButton size="small" :label="`复制 ${source.label} 差异地址`" @click="copyAddress(`diff-${source.method}`)"><Copy class="size-3.5" /></ZIconButton>
        </div>
        <span v-if="copies[`diff-${source.method}`]" class="source-copy-note">{{ copies[`diff-${source.method}`] }}</span>
      </div>
    </div>
    <p class="source-help">仅按需检查默认分支提交，结果复用 5 分钟。仓库变化不等于检测逻辑更新；适配打包后手动安装。</p>
  </section>
</template>

<style scoped>
.source-updates { margin-top: 1.25rem; padding-top: 1rem; border-top: 1px solid var(--cp-color-border-secondary); font-size: .75rem; }
.source-heading, .source-title { display: flex; flex-wrap: wrap; align-items: center; justify-content: space-between; gap: .35rem .5rem; }
.source-heading h3 { display: flex; align-items: center; gap: .45rem; margin: 0; font-size: .8rem; font-weight: 600; }
.source-heading > span, .source-revision, .source-help, .source-copy-note { color: var(--cp-color-text-secondary); }
.source-heading > span { padding: .15rem .4rem; border-radius: var(--cp-border-radius-sm); background: var(--cp-color-fill-quaternary); font-size: .6875rem; }
.source-entry { margin-top: .65rem; }
.source-entry + .source-entry { padding-top: .55rem; border-top: 1px solid var(--cp-color-border-secondary); }
.source-title > div { display: flex; flex-wrap: wrap; align-items: baseline; gap: .3rem .6rem; }
.source-revision { font-family: var(--cp-font-family-mono, monospace); font-size: .6875rem; }
.source-address { display: flex; align-items: center; min-width: 0; gap: .2rem; }
.source-address input { flex: 1; width: 0; min-width: 0; border: 0; border-radius: .25rem; padding: .3rem 0; outline: none; background: transparent; color: var(--cp-color-primary); font: inherit; font-size: .6875rem; text-overflow: ellipsis; }
.source-address input:focus-visible { outline: 2px solid var(--cp-color-control-outline); outline-offset: 2px; }
.source-copy-note { display: block; font-size: .6875rem; }
.source-result, .source-check-error { margin-top: .4rem; padding: .5rem .6rem; border-radius: var(--cp-border-radius-sm); line-height: 1.55; overflow-wrap: anywhere; background: var(--cp-color-fill-quaternary); }
.source-result strong { display: block; font-weight: 500; }
.source-result-identical strong { color: var(--cp-color-success-text); }
.source-result-ahead strong { color: var(--cp-color-primary); }
.source-result-behind strong, .source-result-diverged strong { color: var(--cp-color-warning-text); }
.source-check-error { color: var(--cp-color-error-text); background: var(--cp-color-error-container); }
.source-checked { display: block; color: var(--cp-color-text-secondary); font-size: .6875rem; }
.source-help { margin: .65rem 0 0; font-size: .6875rem; line-height: 1.65; }
</style>
