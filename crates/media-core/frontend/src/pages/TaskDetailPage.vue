<script setup lang="ts">
import { computed, ref } from "vue";
import { useRoute, useRouter } from "vue-router";
import { useQuery } from "@tanstack/vue-query";
import { ElMessage } from "element-plus";

import { taskApi } from "@/shared/api/resources";
import PageHeader from "@/shared/components/PageHeader.vue";
import StatusTag from "@/shared/components/StatusTag.vue";
import { copyText } from "@/shared/utils/clipboard";
import { formatBytes, formatJson, formatTime, shortId } from "@/shared/utils/format";

const route = useRoute();
const router = useRouter();
const taskId = computed(() => String(route.params.id));
const activeTab = ref("overview");

const detailQuery = useQuery({
  queryKey: computed(() => ["task-detail", taskId.value]),
  queryFn: () => taskApi.detail(taskId.value),
});

const eventsQuery = useQuery({
  queryKey: computed(() => ["task-events", taskId.value]),
  queryFn: () => taskApi.events(taskId.value, { page: 1, page_size: 50 }),
});

const logsQuery = useQuery({
  queryKey: computed(() => ["task-logs", taskId.value]),
  queryFn: () => taskApi.logs(taskId.value, {}),
});

const resolvedSpecQuery = useQuery({
  queryKey: computed(() => ["task-resolved-spec", taskId.value]),
  queryFn: () => taskApi.resolvedSpec(taskId.value),
});

function artifactKindLabel(value: string) {
  if (value === "bridge_output") return "桥接输出";
  if (value === "transcode_output") return "转码输出";
  if (value === "stream_ingest_record") return "流接入快录";
  return "—";
}
</script>

<template>
  <section class="page-grid">
    <PageHeader
      :title="detailQuery.data.value?.task.name ?? '任务详情'"
      description="查看任务摘要、当前 Attempt、最近事件、日志，以及任务关联的录像和文件产物。"
    >
      <el-button @click="router.push('/tasks')">返回任务中心</el-button>
      <el-button @click="router.push(`/records?task_id=${taskId}`)">录像中心</el-button>
      <el-button @click="router.push(`/file-artifacts?task_id=${taskId}`)">文件产物</el-button>
    </PageHeader>

    <div v-if="detailQuery.data.value" class="metric-grid">
      <div class="surface-card metric-card">
        <div class="subtle">当前状态</div>
        <strong><StatusTag :status="detailQuery.data.value.task.status" /></strong>
      </div>
      <div class="surface-card metric-card">
        <div class="subtle">当前 Attempt</div>
        <strong>{{ detailQuery.data.value.task.current_attempt_no || "—" }}</strong>
      </div>
      <div class="surface-card metric-card">
        <div class="subtle">执行节点</div>
        <strong>{{ detailQuery.data.value.task.assigned_node_id ?? "—" }}</strong>
      </div>
      <div class="surface-card metric-card">
        <div class="subtle">最近回调</div>
        <strong>{{ detailQuery.data.value.callback_delivery?.event_type ?? "未配置" }}</strong>
      </div>
    </div>

    <div class="surface-card">
      <el-tabs v-model="activeTab">
        <el-tab-pane label="概览" name="overview">
          <el-descriptions :column="2" border v-if="detailQuery.data.value">
            <el-descriptions-item label="任务 ID">{{ detailQuery.data.value.task.id }}</el-descriptions-item>
            <el-descriptions-item label="任务类型">{{ detailQuery.data.value.task.type }}</el-descriptions-item>
            <el-descriptions-item label="创建时间">{{ formatTime(detailQuery.data.value.task.created_at) }}</el-descriptions-item>
            <el-descriptions-item label="更新时间">{{ formatTime(detailQuery.data.value.task.updated_at) }}</el-descriptions-item>
            <el-descriptions-item label="录像数量">{{ detailQuery.data.value.records.length }}</el-descriptions-item>
            <el-descriptions-item label="产物数量">{{ detailQuery.data.value.file_artifacts.length }}</el-descriptions-item>
            <el-descriptions-item label="最近回调状态">{{ detailQuery.data.value.callback_delivery?.status ?? "—" }}</el-descriptions-item>
            <el-descriptions-item label="最近回调错误">{{ detailQuery.data.value.callback_delivery?.last_error ?? "—" }}</el-descriptions-item>
          </el-descriptions>
        </el-tab-pane>

        <el-tab-pane :label="`录像 (${detailQuery.data.value?.records.length ?? 0})`" name="records">
          <div class="table-scroll">
            <el-table v-if="(detailQuery.data.value?.records.length ?? 0) > 0" :data="detailQuery.data.value?.records ?? []">
              <el-table-column label="录像 ID" min-width="120">
                <template #default="{ row }">{{ shortId(row.id) }}</template>
              </el-table-column>
              <el-table-column label="流" min-width="220">
                <template #default="{ row }">{{ [row.vhost, row.app, row.stream].filter(Boolean).join("/") || "—" }}</template>
              </el-table-column>
              <el-table-column prop="file_path" label="文件路径" min-width="320" />
              <el-table-column prop="http_url" label="HTTP 地址" min-width="320" />
              <el-table-column label="大小" min-width="120">
                <template #default="{ row }">{{ formatBytes(row.file_size) }}</template>
              </el-table-column>
              <el-table-column label="时长" min-width="100">
                <template #default="{ row }">{{ row.time_len ? `${row.time_len}s` : "—" }}</template>
              </el-table-column>
              <el-table-column label="开始时间" min-width="180">
                <template #default="{ row }">{{ formatTime(row.start_time ?? row.created_at) }}</template>
              </el-table-column>
              <el-table-column label="操作" min-width="220" fixed="right">
                <template #default="{ row }">
                  <div class="table-actions">
                    <el-button link @click="copyText(row.file_path).then(() => ElMessage.success('已复制文件路径'))">复制路径</el-button>
                    <el-button
                      v-if="row.http_url"
                      link
                      @click="copyText(row.http_url).then(() => ElMessage.success('已复制 HTTP 地址'))"
                    >
                      复制 HTTP 地址
                    </el-button>
                    <el-link v-if="row.http_url" type="primary" :href="row.http_url" target="_blank" rel="noreferrer">打开</el-link>
                  </div>
                </template>
              </el-table-column>
            </el-table>
            <el-empty v-else description="当前任务还没有录像产出" />
          </div>
        </el-tab-pane>

        <el-tab-pane :label="`产物 (${detailQuery.data.value?.file_artifacts.length ?? 0})`" name="artifacts">
          <div class="table-scroll">
            <el-table
              v-if="(detailQuery.data.value?.file_artifacts.length ?? 0) > 0"
              :data="detailQuery.data.value?.file_artifacts ?? []"
            >
              <el-table-column label="产物 ID" min-width="120">
                <template #default="{ row }">{{ shortId(row.id) }}</template>
              </el-table-column>
              <el-table-column label="产物类型" min-width="120">
                <template #default="{ row }">{{ artifactKindLabel(row.artifact_kind) }}</template>
              </el-table-column>
              <el-table-column prop="file_name" label="文件名" min-width="220" />
              <el-table-column prop="file_path" label="文件路径" min-width="320" />
              <el-table-column prop="http_url" label="HTTP 地址" min-width="320" />
              <el-table-column label="大小" min-width="120">
                <template #default="{ row }">{{ formatBytes(row.file_size) }}</template>
              </el-table-column>
              <el-table-column label="创建时间" min-width="180">
                <template #default="{ row }">{{ formatTime(row.created_at) }}</template>
              </el-table-column>
              <el-table-column label="操作" min-width="220" fixed="right">
                <template #default="{ row }">
                  <div class="table-actions">
                    <el-button link @click="copyText(row.file_path).then(() => ElMessage.success('已复制文件路径'))">复制路径</el-button>
                    <el-button link @click="copyText(row.http_url).then(() => ElMessage.success('已复制 HTTP 地址'))">复制 HTTP 地址</el-button>
                    <el-link type="primary" :href="row.http_url" target="_blank" rel="noreferrer">打开</el-link>
                  </div>
                </template>
              </el-table-column>
            </el-table>
            <el-empty v-else description="当前任务还没有文件产物" />
          </div>
        </el-tab-pane>

        <el-tab-pane label="事件" name="events">
          <div class="table-scroll">
            <el-table :data="eventsQuery.data.value?.items ?? []">
              <el-table-column prop="created_at" label="时间" min-width="180">
                <template #default="{ row }">{{ formatTime(row.created_at) }}</template>
              </el-table-column>
              <el-table-column prop="source" label="来源" min-width="120" />
              <el-table-column prop="event_type" label="事件" min-width="200" />
              <el-table-column prop="event_level" label="级别" min-width="120" />
              <el-table-column label="载荷" min-width="360">
                <template #default="{ row }">
                  <pre class="code-block">{{ formatJson(row.payload) }}</pre>
                </template>
              </el-table-column>
            </el-table>
          </div>
        </el-tab-pane>

        <el-tab-pane label="日志" name="logs">
          <div class="table-scroll">
            <el-table :data="logsQuery.data.value?.lines ?? []">
              <el-table-column label="时间" min-width="180">
                <template #default="{ row }">{{ formatTime(row.ts) }}</template>
              </el-table-column>
              <el-table-column prop="stream" label="流" min-width="120" />
              <el-table-column prop="line" label="日志行" min-width="560" />
            </el-table>
          </div>
        </el-tab-pane>

        <el-tab-pane label="requested_spec" name="requested-spec">
          <pre class="code-block">{{ formatJson(detailQuery.data.value?.requested_spec) }}</pre>
        </el-tab-pane>

        <el-tab-pane label="resolved_spec" name="resolved-spec">
          <pre class="code-block">{{ formatJson(resolvedSpecQuery.data.value ?? detailQuery.data.value?.resolved_spec) }}</pre>
        </el-tab-pane>
      </el-tabs>
    </div>
  </section>
</template>
