<script setup lang="ts">
import { computed, reactive, watch } from "vue";
import { useRoute, useRouter } from "vue-router";
import { useMutation, useQuery, useQueryClient } from "@tanstack/vue-query";
import { ElMessage, ElMessageBox } from "element-plus";

import { nodeApi, streamApi } from "@/shared/api/resources";
import OpenInVlcLink from "@/shared/components/OpenInVlcLink.vue";
import PageHeader from "@/shared/components/PageHeader.vue";
import { copyText } from "@/shared/utils/clipboard";
import { errorMessage, formatBitrateKbps, formatTime, shortId } from "@/shared/utils/format";

const route = useRoute();
const router = useRouter();
const queryClient = useQueryClient();

const filters = reactive({
  schema: String(route.query.schema ?? ""),
  app: String(route.query.app ?? ""),
  stream: String(route.query.stream ?? ""),
  task_id: String(route.query.task_id ?? ""),
  node_id: String(route.query.node_id ?? ""),
  has_viewer: String(route.query.has_viewer ?? ""),
});

watch(
  () => route.query,
  (query) => {
    filters.schema = String(query.schema ?? "");
    filters.app = String(query.app ?? "");
    filters.stream = String(query.stream ?? "");
    filters.task_id = String(query.task_id ?? "");
    filters.node_id = String(query.node_id ?? "");
    filters.has_viewer = String(query.has_viewer ?? "");
  },
);

const params = computed(() => ({
  schema: filters.schema,
  app: filters.app,
  stream: filters.stream,
  task_id: filters.task_id,
  node_id: filters.node_id,
  has_viewer: filters.has_viewer,
}));

const streamsQuery = useQuery({
  queryKey: computed(() => ["streams", params.value]),
  queryFn: () => streamApi.list(params.value),
});

const nodesQuery = useQuery({
  queryKey: ["streams", "nodes"],
  queryFn: () => nodeApi.list(),
});

const closeMutation = useMutation({
  mutationFn: (payload: Record<string, unknown>) => streamApi.close(payload),
  onSuccess: () => {
    queryClient.invalidateQueries({ queryKey: ["streams"] });
  },
  onError: (error) => ElMessage.error(errorMessage(error)),
});

const nodeMap = computed(
  () => new Map((nodesQuery.data.value ?? []).map((node) => [node.id, node.node_name])),
);

async function applyFilters() {
  await router.push({
    path: "/streams",
    query: {
      schema: filters.schema || undefined,
      app: filters.app || undefined,
      stream: filters.stream || undefined,
      task_id: filters.task_id || undefined,
      node_id: filters.node_id || undefined,
      has_viewer: filters.has_viewer || undefined,
    },
  });
}

async function resetFilters() {
  filters.schema = "";
  filters.app = "";
  filters.stream = "";
  filters.task_id = "";
  filters.node_id = "";
  filters.has_viewer = "";
  await applyFilters();
}

async function closeStream(row: Record<string, unknown>) {
  await ElMessageBox.confirm(
    `确认关闭 ${row.app}/${row.stream} 吗？这会中断当前内部流和相关播放连接。`,
    "关闭流",
    { type: "warning" },
  );
  await closeMutation.mutateAsync({
    node_id: row.node_id,
    schema: row.schema,
    vhost: row.vhost,
    app: row.app,
    stream: row.stream,
    force: true,
  });
  ElMessage.success("已提交关流请求");
}
</script>

<template>
  <section class="page-grid">
    <PageHeader title="流中心" description="查看当前在线内部流、播放地址、观众状态，以及管理员关流入口。" />

    <div class="surface-card">
      <el-form label-position="top" inline>
        <el-form-item label="协议">
          <el-input v-model="filters.schema" placeholder="rtsp / rtmp / http" />
        </el-form-item>
        <el-form-item label="应用名">
          <el-input v-model="filters.app" placeholder="live" />
        </el-form-item>
        <el-form-item label="流名">
          <el-input v-model="filters.stream" placeholder="camera01" />
        </el-form-item>
        <el-form-item label="任务 ID">
          <el-input v-model="filters.task_id" placeholder="可选" />
        </el-form-item>
        <el-form-item label="节点">
          <el-select v-model="filters.node_id" clearable filterable style="width: 220px">
            <el-option
              v-for="node in nodesQuery.data.value ?? []"
              :key="node.id"
              :label="node.node_name"
              :value="node.id"
            />
          </el-select>
        </el-form-item>
        <el-form-item label="有观众">
          <el-select v-model="filters.has_viewer" clearable style="width: 140px">
            <el-option label="是" value="true" />
            <el-option label="否" value="false" />
          </el-select>
        </el-form-item>
        <el-form-item>
          <el-button type="primary" @click="applyFilters">应用筛选</el-button>
          <el-button @click="resetFilters">重置</el-button>
        </el-form-item>
      </el-form>
    </div>

    <div class="surface-card">
      <div class="section-stack" style="gap: 8px; margin-bottom: 16px">
        <h3 class="page-section-title">在线内部流</h3>
        <p class="subtle">播放地址表示同一条内部流当前可暴露的协议集合，不代表任务额外创建了多个独立目标。</p>
      </div>

      <div class="table-scroll">
        <el-table :data="streamsQuery.data.value ?? []" v-loading="streamsQuery.isLoading.value">
        <el-table-column label="协议" min-width="120">
          <template #default="{ row }">
            <el-tag round>{{ row.schema }}</el-tag>
          </template>
        </el-table-column>
        <el-table-column label="Vhost / 应用 / 流" min-width="260">
          <template #default="{ row }">
            <div><strong>{{ row.vhost }}</strong></div>
            <div class="subtle">{{ row.app }}/{{ row.stream }}</div>
          </template>
        </el-table-column>
        <el-table-column label="任务" min-width="220">
          <template #default="{ row }">
            <div>
              <el-link type="primary" @click="router.push(`/tasks/${row.task_id}`)">{{ row.task_name || shortId(row.task_id) }}</el-link>
            </div>
            <div class="subtle">{{ shortId(row.task_id) }}</div>
          </template>
        </el-table-column>
        <el-table-column label="节点" min-width="160">
          <template #default="{ row }">{{ nodeMap.get(row.node_id) ?? "—" }}</template>
        </el-table-column>
        <el-table-column label="观众数" min-width="100">
          <template #default="{ row }">
            {{
              row.viewer_count ?? (row.has_viewer === true ? "至少 1" : row.has_viewer === false ? 0 : "—")
            }}
          </template>
        </el-table-column>
        <el-table-column label="最近码率" min-width="120">
          <template #default="{ row }">{{ formatBitrateKbps(row.bitrate_kbps) }}</template>
        </el-table-column>
        <el-table-column label="开始时间" min-width="180">
          <template #default="{ row }">{{ formatTime(row.started_at) }}</template>
        </el-table-column>
        <el-table-column label="播放地址" min-width="340">
          <template #default="{ row }">
            <div class="stack-inline-links">
              <OpenInVlcLink
                v-for="url in row.play_urls"
                :key="url"
                :url="url"
              >
                {{ url }}
              </OpenInVlcLink>
            </div>
          </template>
        </el-table-column>
        <el-table-column label="操作" min-width="180" fixed="right">
          <template #default="{ row }">
            <div class="table-actions">
              <el-button link type="primary" @click="router.push(`/tasks/${row.task_id}`)">任务</el-button>
              <el-button v-if="row.play_urls?.[0]" link @click="copyText(row.play_urls[0]).then(() => ElMessage.success('已复制播放地址'))">
                复制地址
              </el-button>
              <el-button v-if="row.node_id" link type="danger" @click="closeStream(row)">关流</el-button>
            </div>
          </template>
        </el-table-column>
        </el-table>
      </div>
    </div>
  </section>
</template>
