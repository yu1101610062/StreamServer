<script setup lang="ts">
import { reactive, ref } from "vue";
import { useQuery } from "@tanstack/vue-query";
import { ElMessage } from "element-plus";

import { debugApi, nodeApi } from "@/shared/api/resources";
import type { HookEventSummary, UnknownJson } from "@/shared/api/types";
import PageHeader from "@/shared/components/PageHeader.vue";
import { errorMessage, formatJson, formatTime } from "@/shared/utils/format";

const nodesQuery = useQuery({
  queryKey: ["debug", "nodes"],
  queryFn: () => nodeApi.list(),
});

const selectedNodeId = ref("");
const statisticResult = ref<UnknownJson | null>(null);
const threadsLoadResult = ref<UnknownJson | null>(null);
const workThreadsLoadResult = ref<UnknownJson | null>(null);
const mediaResult = ref<UnknownJson | null>(null);
const sessionsResult = ref<UnknownJson | null>(null);
const playersResult = ref<UnknownJson | null>(null);
const hooksResult = ref<HookEventSummary[]>([]);
const snapResult = ref<{ data_url: string } | null>(null);
const loading = reactive({
  statistic: false,
  media: false,
  sessions: false,
  players: false,
  hooks: false,
  snap: false,
});

const mediaFilters = reactive({
  schema: "",
  vhost: "__defaultVhost__",
  app: "live",
  stream: "",
});

const kickSessionForm = reactive({
  session_id: "",
});

const kickBatchForm = reactive({
  local_port: "",
  peer_ip: "",
});

const closeForm = reactive({
  schema: "",
  vhost: "__defaultVhost__",
  app: "live",
  stream: "",
  force: true,
});

const snapForm = reactive({
  url: "",
  timeout_sec: "10",
  expire_sec: "30",
});

function ensureNode() {
  if (!selectedNodeId.value) {
    throw new Error("请先选择节点");
  }
  return selectedNodeId.value;
}

async function loadStatistic() {
  try {
    loading.statistic = true;
    const node_id = ensureNode();
    const [statistic, threadsLoad, workThreadsLoad] = await Promise.all([
      debugApi.statistic({ node_id }),
      debugApi.threadsLoad({ node_id }),
      debugApi.workThreadsLoad({ node_id }),
    ]);
    statisticResult.value = statistic;
    threadsLoadResult.value = threadsLoad;
    workThreadsLoadResult.value = workThreadsLoad;
  } catch (error) {
    ElMessage.error(errorMessage(error));
  } finally {
    loading.statistic = false;
  }
}

async function loadMedia() {
  try {
    loading.media = true;
    const node_id = ensureNode();
    mediaResult.value = await debugApi.media({
      node_id,
      schema: mediaFilters.schema,
      vhost: mediaFilters.vhost,
      app: mediaFilters.app,
      stream: mediaFilters.stream,
    });
  } catch (error) {
    ElMessage.error(errorMessage(error));
  } finally {
    loading.media = false;
  }
}

async function loadSessions() {
  try {
    loading.sessions = true;
    const node_id = ensureNode();
    sessionsResult.value = await debugApi.sessions({ node_id });
  } catch (error) {
    ElMessage.error(errorMessage(error));
  } finally {
    loading.sessions = false;
  }
}

async function loadPlayers() {
  try {
    loading.players = true;
    const node_id = ensureNode();
    playersResult.value = await debugApi.players({ node_id });
  } catch (error) {
    ElMessage.error(errorMessage(error));
  } finally {
    loading.players = false;
  }
}

async function loadHooks() {
  try {
    loading.hooks = true;
    const node_id = ensureNode();
    hooksResult.value = await debugApi.hooks({ node_id });
  } catch (error) {
    ElMessage.error(errorMessage(error));
  } finally {
    loading.hooks = false;
  }
}

async function kickSession() {
  try {
    await debugApi.kickSession({ node_id: ensureNode(), session_id: kickSessionForm.session_id });
    ElMessage.success("已提交踢会话请求");
  } catch (error) {
    ElMessage.error(errorMessage(error));
  }
}

async function kickSessions() {
  try {
    await debugApi.kickSessions({
      node_id: ensureNode(),
      local_port: kickBatchForm.local_port ? Number(kickBatchForm.local_port) : undefined,
      peer_ip: kickBatchForm.peer_ip || undefined,
    });
    ElMessage.success("已提交批量踢会话请求");
  } catch (error) {
    ElMessage.error(errorMessage(error));
  }
}

async function closeStream() {
  try {
    await debugApi.closeStream({
      node_id: ensureNode(),
      schema: closeForm.schema,
      vhost: closeForm.vhost,
      app: closeForm.app,
      stream: closeForm.stream,
      force: closeForm.force,
    });
    ElMessage.success("已提交关流请求");
  } catch (error) {
    ElMessage.error(errorMessage(error));
  }
}

async function snap() {
  try {
    loading.snap = true;
    snapResult.value = await debugApi.snap({
      node_id: ensureNode(),
      url: snapForm.url,
      timeout_sec: snapForm.timeout_sec ? Number(snapForm.timeout_sec) : undefined,
      expire_sec: snapForm.expire_sec ? Number(snapForm.expire_sec) : undefined,
    });
  } catch (error) {
    ElMessage.error(errorMessage(error));
  } finally {
    loading.snap = false;
  }
}
</script>

<template>
  <section class="page-grid">
    <PageHeader title="调试台" description="管理员专用，封装 ZLM 媒体列表、会话、玩家、关流、抓图和 Hook 排障入口。" />

    <div class="surface-card">
      <el-form label-position="top" inline>
        <el-form-item label="节点">
          <el-select v-model="selectedNodeId" filterable clearable placeholder="请选择节点" style="width: 260px">
            <el-option v-for="node in nodesQuery.data.value ?? []" :key="node.id" :label="node.node_name" :value="node.id" />
          </el-select>
        </el-form-item>
      </el-form>
      <p class="subtle">先选择节点，再执行统计、查询、踢人、关流和抓图操作。</p>
    </div>

    <div class="surface-card">
      <div class="table-actions" style="justify-content: space-between; margin-bottom: 16px">
        <div>
          <h3 class="page-section-title">ZLM 统计</h3>
          <p class="subtle">对象统计、前台线程负载和后台线程负载。</p>
        </div>
        <el-button type="primary" :loading="loading.statistic" @click="loadStatistic">加载统计</el-button>
      </div>
      <el-row :gutter="16">
        <el-col :md="8" :span="24">
          <pre class="code-block">{{ formatJson(statisticResult) }}</pre>
        </el-col>
        <el-col :md="8" :span="24">
          <pre class="code-block">{{ formatJson(threadsLoadResult) }}</pre>
        </el-col>
        <el-col :md="8" :span="24">
          <pre class="code-block">{{ formatJson(workThreadsLoadResult) }}</pre>
        </el-col>
      </el-row>
    </div>

    <div class="surface-card">
      <div class="table-actions" style="justify-content: space-between; margin-bottom: 16px">
        <div>
          <h3 class="page-section-title">媒体列表</h3>
          <p class="subtle">按 schema / vhost / app / stream 查询。</p>
        </div>
        <el-button type="primary" :loading="loading.media" @click="loadMedia">查询媒体</el-button>
      </div>
      <el-form label-position="top" inline>
        <el-form-item label="协议">
          <el-input v-model="mediaFilters.schema" placeholder="rtsp / rtmp / http" />
        </el-form-item>
        <el-form-item label="Vhost">
          <el-input v-model="mediaFilters.vhost" placeholder="__defaultVhost__" />
        </el-form-item>
        <el-form-item label="应用名">
          <el-input v-model="mediaFilters.app" placeholder="live" />
        </el-form-item>
        <el-form-item label="流名">
          <el-input v-model="mediaFilters.stream" placeholder="camera01" />
        </el-form-item>
      </el-form>
      <pre class="code-block">{{ formatJson(mediaResult) }}</pre>
    </div>

    <div class="surface-card">
      <div class="table-actions" style="justify-content: space-between; margin-bottom: 16px">
        <div>
          <h3 class="page-section-title">会话与播放器</h3>
          <p class="subtle">读取 ZLM 的全部会话和播放器列表。</p>
        </div>
        <div class="table-actions">
          <el-button type="primary" :loading="loading.sessions" @click="loadSessions">查询会话</el-button>
          <el-button :loading="loading.players" @click="loadPlayers">查询播放器</el-button>
        </div>
      </div>
      <el-row :gutter="16">
        <el-col :md="12" :span="24">
          <pre class="code-block">{{ formatJson(sessionsResult) }}</pre>
        </el-col>
        <el-col :md="12" :span="24">
          <pre class="code-block">{{ formatJson(playersResult) }}</pre>
        </el-col>
      </el-row>
    </div>

    <div class="surface-card">
      <h3 class="page-section-title">执行动作</h3>
      <el-row :gutter="16">
        <el-col :md="12" :span="24">
          <el-form label-position="top">
            <el-form-item label="单个踢会话">
              <el-input v-model="kickSessionForm.session_id" placeholder="session_id" />
            </el-form-item>
            <el-button type="danger" @click="kickSession">踢会话</el-button>
          </el-form>
        </el-col>
        <el-col :md="12" :span="24">
          <el-form label-position="top">
            <el-form-item label="批量踢会话 - 本地端口">
              <el-input v-model="kickBatchForm.local_port" placeholder="例如 554" />
            </el-form-item>
            <el-form-item label="批量踢会话 - 对端 IP">
              <el-input v-model="kickBatchForm.peer_ip" placeholder="例如 10.0.0.8" />
            </el-form-item>
            <el-button type="danger" plain @click="kickSessions">批量踢会话</el-button>
          </el-form>
        </el-col>
      </el-row>

      <el-divider />

      <el-row :gutter="16">
        <el-col :md="12" :span="24">
          <el-form label-position="top">
            <el-form-item label="关闭流 - 协议">
              <el-input v-model="closeForm.schema" placeholder="rtsp / rtmp / http" />
            </el-form-item>
            <el-form-item label="关闭流 - Vhost">
              <el-input v-model="closeForm.vhost" placeholder="__defaultVhost__" />
            </el-form-item>
            <el-form-item label="关闭流 - 应用名">
              <el-input v-model="closeForm.app" placeholder="live" />
            </el-form-item>
            <el-form-item label="关闭流 - 流名">
              <el-input v-model="closeForm.stream" placeholder="camera01" />
            </el-form-item>
            <el-checkbox v-model="closeForm.force">强制关闭</el-checkbox>
            <div style="margin-top: 12px">
              <el-button type="danger" @click="closeStream">关闭流</el-button>
            </div>
          </el-form>
        </el-col>
        <el-col :md="12" :span="24">
          <el-form label-position="top">
            <el-form-item label="截图地址">
              <el-input v-model="snapForm.url" placeholder="rtsp://127.0.0.1/live/camera01" />
            </el-form-item>
            <el-form-item label="超时（秒）">
              <el-input v-model="snapForm.timeout_sec" />
            </el-form-item>
            <el-form-item label="保留（秒）">
              <el-input v-model="snapForm.expire_sec" />
            </el-form-item>
            <el-button type="primary" :loading="loading.snap" @click="snap">抓图</el-button>
          </el-form>
          <img v-if="snapResult?.data_url" :src="snapResult.data_url" alt="ZLM 抓图预览" class="snap-preview" />
        </el-col>
      </el-row>
    </div>

    <div class="surface-card">
      <div class="table-actions" style="justify-content: space-between; margin-bottom: 16px">
        <div>
          <h3 class="page-section-title">Hook 时间线</h3>
          <p class="subtle">查看最近收到的 Hook 事件和去重处理状态。</p>
        </div>
        <el-button type="primary" :loading="loading.hooks" @click="loadHooks">加载 Hook 时间线</el-button>
      </div>

      <el-timeline>
        <el-timeline-item
          v-for="item in hooksResult"
          :key="item.id"
          :timestamp="formatTime(item.created_at)"
          placement="top"
        >
          <div class="surface-panel" style="padding: 16px">
            <strong>{{ item.hook_name }}</strong>
            <div class="subtle">dedup_key: {{ item.dedup_key }}</div>
            <pre class="code-block">{{ formatJson(item.payload) }}</pre>
          </div>
        </el-timeline-item>
      </el-timeline>
    </div>
  </section>
</template>
