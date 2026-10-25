<script setup lang="ts">
import { computed, onMounted, onScopeDispose, ref, watch } from 'vue'
import { ZButton, ZCard, ZSelect, ZSegmented, ZTable, ZTag, ZIconButton } from '@codex-proxy/ui'
import type { TableColumn } from '@codex-proxy/ui'
import { Play, Pause, Square, RefreshCw, FlaskConical, ArrowRight, Check, Fingerprint, ShieldCheck } from '@lucide/vue'
import { api } from './api'
import type { Bootstrap, Run, History, Method } from './api'
import { accountDisplayName } from './accountDisplay'
import { formatTime, methodText, statusText, tierText, verdict, tone, reasonText } from './presentation'
import SourceUpdates from './SourceUpdates.vue'

const preview = import.meta.env.DEV && new URLSearchParams(location.search).get('preview') === '1'
const bootstrap = ref<Bootstrap | null>(null), history = ref<History[]>([]), run = ref<Run | null>(null)
const loading = ref(true), error = ref(''), action = ref(false), pumping = ref(false), modelsLoading = ref(false), starting = ref(false)
const account = ref(''), key = ref(''), model = ref(''), claimed = ref(''), tier = ref('low'), models = ref<string[]>([])
const method = ref<Method>('meow')
const catalog = computed(() => bootstrap.value?.catalog.methods.find(c => c.method === method.value))
const methodOptions = computed(() => bootstrap.value?.catalog.methods.map(c => ({value:c.method,label:c.label})) ?? [])
watch(method, value => { tier.value = value === 'modeltrace' ? 'fixed' : 'low'; claimed.value = value === 'modeltrace' ? 'unified' : '' })
const reportView = ref('fingerprint')
let keepRunning = false, disposed = false, modelSequence = 0
const tierOptions = computed(() => catalog.value?.tiers.map(t => ({value:t.id,label:`${({low:'快速',medium:'标准',high:'完整'} as Record<string,string>)[t.id]} · ${t.count} 题`})) ?? [])
const count = computed(() => catalog.value?.tiers.find(t => t.id === tier.value)?.count ?? 0)
const finished = computed(() => run.value?.status === 'completed' || run.value?.status === 'stopped')
const valid = computed(() => run.value?.attempts.filter(a => a.status === 'ok').length ?? 0)
const failures = computed(() => run.value?.attempts.filter(a => a.status === 'error').length ?? 0)
const canStart = computed(() => account.value && key.value && model.value && claimed.value && !loading.value && !modelsLoading.value && !action.value && !pumping.value && !error.value)
const lastError = computed(() => [...(run.value?.attempts ?? [])].reverse().find(a => a.status === 'error')?.error)
const traceReport = computed(() => run.value?.report.method === 'modeltrace' ? run.value.report : null)
const candidates = computed<[string, number][]>(() => {
  const report = run.value?.report
  return report?.method === 'modeltrace' ? (report.analysis?.results.slice(0,5).map(r => [r.display_name,r.probability] as [string,number]) ?? []) : Object.entries(report?.matches ?? {}).sort((a,b) => b[1]-a[1])
})
const historyColumns: TableColumn<History>[] = [{key:'created_at',label:'检测时间',kind:'datetime',formatter:value=>formatTime(Number(value))},{key:'account_name',label:'账号',kind:'identity',size:'xl'},{key:'method',label:'检测方法',size:'lg',formatter:value=>methodText(value as Method | undefined)},{key:'request_model',label:'请求模型',kind:'mono',size:'lg'},{key:'tier',label:'检测档位',size:'lg',formatter:value=>tierText[String(value)]},{key:'result',label:'检测结果',kind:'custom',size:'xl'},{key:'actions',label:'操作',kind:'actions'}]
const attemptColumns = [{key:'number',label:'#',kind:'index'},{key:'job_id',label:'样本',kind:'mono',size:'xl'},{key:'status',label:'状态',kind:'status'},{key:'answer',label:'回答 / 错误',kind:'custom',size:'2xl'},{key:'request_id',label:'请求 ID',kind:'mono',size:'2xl'}] satisfies TableColumn[]
const accountOptions = computed(() => bootstrap.value?.accounts.map(a => ({value:a.id,label:accountDisplayName(a)})) ?? [])

function message(cause: unknown) { return cause instanceof Error ? cause.message : String(cause) }
async function loadHistory() { history.value = (await api<{runs: History[]}>('runs')).runs }
async function load() {
  loading.value = true; error.value = ''
  try { bootstrap.value = await api<Bootstrap>('bootstrap'); await loadHistory(); if (key.value) await loadModels(key.value) }
  catch (cause) { error.value = message(cause) }
  finally { loading.value = false }
}
async function loadModels(value: string) {
  const sequence = ++modelSequence
  model.value = ''; models.value = []; error.value = ''
  if (!value) return
  modelsLoading.value = true
  try { const result = await api<{models: string[]}>('models', {key_id:value}); if (sequence === modelSequence) models.value = result.models }
  catch (cause) { if (sequence === modelSequence) error.value = message(cause) }
  finally { if (sequence === modelSequence) modelsLoading.value = false }
}
watch(key, value => { void loadModels(value) })
async function operate(fn: () => Promise<void>) {
  if (action.value) return
  action.value = true; error.value = ''
  try { await fn() } catch (cause) { error.value = message(cause); keepRunning = false }
  finally { action.value = false }
}
async function pump() {
  if (pumping.value || !run.value) return
  const id = run.value.id
  keepRunning = true; pumping.value = true
  try {
    while (keepRunning && !disposed && document.visibilityState === 'visible' && run.value?.id === id && run.value.status === 'ready') {
      run.value = (await api<{run: Run}>('runs/step', {id})).run
    }
    if (!disposed) await loadHistory()
  } catch (cause) { error.value = `${message(cause)}；请刷新任务状态后再续测，避免重复请求` }
  finally { pumping.value = false; keepRunning = false }
}
async function start() {
  starting.value = true
  await operate(async () => {
    run.value = (await api<{run: Run}>('runs/start', {method:method.value,account_id:account.value,key_id:key.value,request_model:model.value,claimed_model:claimed.value,tier:tier.value})).run
    reportView.value = 'fingerprint'
    await loadHistory()
  })
  starting.value = false
  if (!error.value) void pump()
}
async function control(command: string) {
  if (!run.value) return
  keepRunning = false
  await operate(async () => { run.value = (await api<{run: Run}>('runs/control', {id:run.value!.id,action:command})).run; await loadHistory() })
  if (command === 'resume' && !error.value) void pump()
}
async function openRun(id: string) { await operate(async () => { run.value = (await api<{run: Run}>('runs/get',{id})).run }) }
function visibility() { if (document.visibilityState !== 'visible') keepRunning = false }
onMounted(() => { void load(); document.addEventListener('visibilitychange',visibility) })
onScopeDispose(() => { disposed=true;keepRunning=false;document.removeEventListener('visibilitychange',visibility) })
</script>

<template>
  <main class="flex min-w-0 flex-col gap-5 font-sans text-cp-text">
    <div v-if="preview" class="preview-note" role="note">独立模拟预览 · 不代表真实账号的检测结果</div>
    <div class="flex flex-wrap items-center justify-between gap-3">
      <p class="m-0 text-cp-sm text-cp-text-secondary">固定账号 · 原生代理链路 · 双方法行为指纹</p>
      <ZIconButton label="刷新账号和历史" :disabled="loading || pumping" @click="load"><RefreshCw class="size-4" /></ZIconButton>
    </div>
    <div v-if="error" class="error-panel" role="alert"><strong>操作未完成</strong><p>{{ error }}</p><ZButton size="small" @click="error=''; run ? openRun(run.id) : load()">重新读取状态</ZButton></div>
    <div class="setup-grid">
      <ZCard>
        <template #header><div class="flex items-center gap-3"><span class="section-icon"><FlaskConical class="size-5" /></span><div><h2>新建检测</h2><p class="subtitle">使用已有账号，无需填写 API 地址或密钥</p></div></div></template>
        <template #body>
          <p v-if="loading" role="status" class="muted">正在读取宿主账号与检测基线…</p>
          <p v-else-if="bootstrap && (!bootstrap.accounts.length || !bootstrap.keys.length)" role="status" class="muted">请先在主项目启用 OpenAI 账号和测试 Key，然后刷新。</p>
          <div class="form-grid">
            <div class="field col-span-full"><label id="method-label">检测方法</label><ZSegmented v-model="method" aria-labelledby="method-label" label="检测方法" :options="methodOptions" :disabled="loading || pumping || action" /></div>
            <div class="field"><label id="account-label">检测账号</label><ZSelect v-model="account" aria-labelledby="account-label" :options="accountOptions" :disabled="loading || pumping" placeholder="选择已有账号" searchable /></div>
            <div class="field"><label id="key-label">计量 Key</label><ZSelect v-model="key" aria-labelledby="key-label" :options="bootstrap?.keys.map(k=>({value:k.id,label:k.name})) ?? []" :disabled="loading || pumping" placeholder="选择用于计量的 Key" searchable /></div>
            <div class="field"><label id="model-label">请求模型</label><ZSelect v-model="model" aria-labelledby="model-label" :options="models.map(m=>({value:m,label:m}))" :loading="modelsLoading" :disabled="!key || pumping" placeholder="先选择计量 Key" searchable /></div>
            <div class="field"><label id="claimed-label">对照基线</label><ZSelect v-if="method === 'meow'" v-model="claimed" aria-labelledby="claimed-label" :options="catalog?.models.map(m=>({value:m.id,label:m.label || m.id})) ?? []" :disabled="loading || pumping" placeholder="选择要核对的模型" /><ZSelect v-else model-value="unified" aria-labelledby="claimed-label" :options="[{value:'unified',label:`统一候选库 · ${catalog?.models.length ?? 0} 个模型`}]" disabled /></div>
            <div v-if="method === 'meow'" class="field col-span-full"><label id="tier-label">检测档位</label><ZSegmented v-model="tier" aria-labelledby="tier-label" label="检测档位" :options="tierOptions" :disabled="pumping" /></div>
            <p v-else class="col-span-full muted text-cp-sm m-0">三轮独立数字选择 · 同时比较 GPT / Claude 家族候选</p>
          </div>
          <div class="start-row"><p class="muted text-cp-xs">{{ count }} 个样本 · 最多 {{ count + Math.ceil(count/2) }} 次请求（含重试）<br>真实调用会计入所选 Key 的用量；每次仅检测一个请求。</p><ZButton type="primary" :disabled="!canStart" :loading="starting" @click="start"><Play class="size-4" />开始检测</ZButton></div>
        </template>
      </ZCard>
      <ZCard>
        <template #header><div class="flex items-center gap-2"><Fingerprint class="size-5 text-cp-text-secondary" /><h2>如何理解结果</h2></div></template>
        <template #body>
          <div class="explanation"><p>这里比较的是<span class="text-cp-text font-emphasis">模型行为指纹</span>，不是智商分数，也不是模型身份认证</p><ul><li><Check class="size-4" /><span>锁定所选账号，由主服务使用其原有代理和凭据发送</span></li><li><Check class="size-4" /><span>错误、拒答和截断不算模型答错，失败后自动暂停</span></li><li><Check class="size-4" /><span>切走页面后不再发下一题，重新打开可手动续测</span></li></ul><div class="baseline-meta"><span>检测引擎</span><strong>{{ methodText(method) }} {{ catalog?.engine_version ?? '—' }}</strong><span>基线版本</span><strong>{{ catalog?.version ?? '—' }}</strong></div><p v-if="method === 'modeltrace'" class="text-cp-xs mb-0">概率只在候选库内比较，库外模型也会归到最接近的候选，不代表真实身份概率</p><p v-else class="text-cp-xs mb-0">Meow 使用 GPT 行为基线，检测不会自动调整账号、路由或额度</p></div>
          <SourceUpdates v-if="bootstrap" :sources="bootstrap.sources" />
        </template>
      </ZCard>
    </div>

    <ZCard v-if="run">
      <template #header><div class="flex flex-wrap items-center justify-between gap-3"><div><div class="flex flex-wrap items-center gap-2"><h2>{{ run.account_name }}</h2><ZTag :type="tone(run.report,run.status)">{{ statusText[run.status] }}</ZTag><ZTag>{{ methodText(run.plan.method) }}</ZTag></div><p class="subtitle">{{ run.plan.request_model }} <span class="mx-1">→</span> {{ run.plan.method === 'modeltrace' ? '统一候选库' : run.plan.claimed_model }} · {{ tierText[run.plan.tier] }}</p></div><div class="flex flex-wrap gap-2"><ZIconButton label="刷新当前任务" :disabled="action || pumping" @click="openRun(run.id)"><RefreshCw class="size-4" /></ZIconButton><template v-if="!finished"><ZButton v-if="pumping" size="small" :disabled="action" @click="control('pause')"><Pause class="size-4" />当前题完成后暂停</ZButton><ZButton v-else size="small" type="primary" :disabled="action" @click="control('resume')"><Play class="size-4" />继续检测</ZButton><ZButton size="small" :disabled="action" @click="control('stop')"><Square class="size-3" />结束检测</ZButton></template></div></div></template>
      <template #body>
        <div class="progress-facts"><div><span>有效样本</span><strong>{{ valid }} <small>/ {{ run.plan.jobs.length }}</small></strong></div><div><span>尝试（含中断）</span><strong>{{ run.attempts.length }} <small>/ {{ run.plan.jobs.length + run.plan.retry_budget }}</small></strong></div><div><span>失败 / 中断</span><strong>{{ failures }}</strong></div><div><span>检测结论</span><strong class="verdict-text">{{ verdict(run.report,run.status) }}</strong></div></div>
        <progress class="sample-progress" :value="valid" :max="run.plan.jobs.length" aria-label="有效样本进度" />
        <p v-if="pumping" role="status" class="muted text-cp-sm">当前题目正在请求中；结束或暂停会保留本题的用量与结果。</p>
        <p v-else-if="!finished" class="muted text-cp-sm">检测未自动运行。点击“继续检测”后，页面会逐题发送请求。</p>
        <p v-if="lastError" class="sample-error" role="status">最近一次异常：{{ lastError }}</p>
        <div class="flex flex-wrap justify-between items-center gap-3 mt-5 mb-4"><ZSegmented v-model="reportView" label="报告视图" :options="[{value:'fingerprint',label:'指纹报告'},{value:'samples',label:'请求明细'}]" /><span class="muted text-cp-xs">{{ formatTime(run.updated_at) }} 更新 · {{ run.key_name }}</span></div>
        <div v-if="reportView === 'fingerprint'" class="report-grid">
          <div><p class="text-cp-sm font-emphasis mt-0">{{ traceReport ? '前 5 候选 · 库内相对概率' : '候选相对匹配度' }} <span class="muted font-normal">（不是正确率）</span></p><p v-if="!candidates.length" class="muted">还没有有效样本</p><div v-for="[name,value] in candidates" :key="name" class="candidate"><div class="flex justify-between gap-3 text-cp-sm"><span class="break-all">{{ name === 'other_known_external' ? '其他 / 未建模行为' : name }}</span><span class="font-mono">{{ (value*100).toFixed(1) }}%</span></div><div class="candidate-track"><div :style="{width:`${Math.max(0,Math.min(100,value*100))}%`}" /></div><p v-if="run.report.method !== 'modeltrace'" class="muted text-cp-xs mt-1 mb-0">判定阈值 {{ ((run.report.thresholds[name] ?? 0)*100).toFixed(1) }}%</p></div><div v-if="traceReport?.analysis" class="baseline-meta"><template v-for="family in traceReport.analysis.family_probabilities" :key="family.family"><span>{{ family.display_name }} 家族</span><strong>{{ (family.probability*100).toFixed(1) }}%</strong></template></div></div>
          <div class="report-note"><ShieldCheck class="size-5 mb-3 text-cp-text-secondary" /><strong>{{ verdict(run.report,run.status) }}</strong><p v-if="run.report.reasons.length">{{ run.report.reasons.map(r=>reasonText[r] || r).join('，') }}</p><p v-if="traceReport">ModelTrace 在冻结的统一候选库内比较，库外模型也会被归到现有候选，结果不是模型身份认证</p><p v-else-if="!run.report.reasons.length">结果仅针对本次账号、请求模型与冻结的基线，建议同条件复测后再判断是否存在持续偏移</p><p>请求模型可能经过主项目的模型映射或中间件，报告反映这条实际链路的行为</p><p class="text-cp-xs mb-1">{{ methodText(run.plan.method) }} · {{ run.plan.version }} · 冻结基线 SHA-256</p><code class="baseline-hash">{{ run.plan.hash }}</code></div>
        </div>
        <ZTable v-else :columns="attemptColumns" :data="run.attempts.map((a,i)=>({...a,number:i+1,id:i}))" empty-text="尚未发起检测请求"><template #status="{row}"><ZTag :type="row.status === 'ok' ? 'success' : row.status === 'pending' ? 'primary' : 'warning'">{{ row.status === 'ok' ? '有效' : row.status === 'pending' ? '进行中' : '异常' }}</ZTag></template><template #answer="{row}"><span class="sample-answer">{{ row.error || row.answer || '—' }}{{ row.answer_truncated ? '（展示已截短，评分使用完整回答）' : '' }}</span></template></ZTable>
      </template>
    </ZCard>
    <ZCard>
      <template #header><div class="flex flex-wrap justify-between gap-3"><div><h2>检测历史</h2><p class="subtitle">保留最近 16 条记录 · 报告沿用检测时的基线，不重新评分</p></div><ZTag class="self-start">{{ history.length }} 条</ZTag></div></template>
      <template #body><ZTable :columns="historyColumns" :data="history" :loading="loading" empty-text="还没有检测记录。选择账号，开始第一次检测。"><template #result="{row}"><ZTag :type="tone(row.report,row.status)">{{ row.status === 'completed' || row.status === 'stopped' ? verdict(row.report,row.status) : statusText[row.status] }}</ZTag></template><template #actions="{row}"><ZIconButton :label="`查看 ${row.account_name} 的检测报告`" :disabled="pumping || action" @click="openRun(row.id)"><ArrowRight class="size-4" /></ZIconButton></template></ZTable></template>
    </ZCard>
  </main>
</template>
