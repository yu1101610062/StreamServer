<script setup lang="ts">
import { computed, ref } from "vue";
import { useRoute, useRouter } from "vue-router";
import { useQuery } from "@tanstack/vue-query";

import { taskApi } from "@/shared/api/resources";
import PageHeader from "@/shared/components/PageHeader.vue";
import StatusTag from "@/shared/components/StatusTag.vue";
import { formatJson, formatTime } from "@/shared/utils/format";

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
</script>

<template>
  <section class="page-grid">
    <PageHeader
      :title="detailQuery.data.value?.task.name ?? '任务详情'"
      description="查看任务摘要、当前 Attempt、最近事件、日志以及请求/解析规格。"
    >
      <el-button @click="router.push('/tasks')">返回任务中心</el-button>
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
            <el-descriptions-item label="最近回调状态">{{ detailQuery.data.value.callback_delivery?.status ?? "—" }}</el-descriptions-item>
            <el-descriptions-item label="最近回调错误">{{ detailQuery.data.value.callback_delivery?.last_error ?? "—" }}</el-descriptions-item>
          </el-descriptions>
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
