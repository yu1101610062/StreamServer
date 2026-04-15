<script setup lang="ts">
import { computed, reactive, watch } from "vue";
import { useRoute, useRouter } from "vue-router";
import { useMutation, useQuery, useQueryClient } from "@tanstack/vue-query";
import { ElMessage, ElMessageBox } from "element-plus";

import { nodeApi, taskApi } from "@/shared/api/resources";
import PageHeader from "@/shared/components/PageHeader.vue";
import StatusTag from "@/shared/components/StatusTag.vue";
import { TASK_TYPES, taskTypeLabel } from "@/shared/labels";
import type { TaskSummary } from "@/shared/api/types";
import { errorMessage, formatTime, shortId } from "@/shared/utils/format";

const router = useRouter();
const route = useRoute();
const queryClient = useQueryClient();

const filters = reactive({
  status: String(route.query.status ?? ""),
  type: String(route.query.type ?? ""),
  assigned_node_id: String(route.query.assigned_node_id ?? ""),
  keyword: String(route.query.keyword ?? ""),
  created_from: String(route.query.created_from ?? ""),
  created_to: String(route.query.created_to ?? ""),
  page: Number(route.query.page ?? 1),
  page_size: Number(route.query.page_size ?? 20),
  sort_by: String(route.query.sort_by ?? "created_at"),
  sort_order: String(route.query.sort_order ?? "desc"),
});

watch(
  () => route.query,
  (query) => {
    filters.status = String(query.status ?? "");
    filters.type = String(query.type ?? "");
    filters.assigned_node_id = String(query.assigned_node_id ?? "");
    filters.keyword = String(query.keyword ?? "");
    filters.created_from = String(query.created_from ?? "");
    filters.created_to = String(query.created_to ?? "");
    filters.page = Number(query.page ?? 1);
    filters.page_size = Number(query.page_size ?? 20);
    filters.sort_by = String(query.sort_by ?? "created_at");
    filters.sort_order = String(query.sort_order ?? "desc");
  },
);

const taskParams = computed(() => ({
  status: filters.status,
  type: filters.type,
  assigned_node_id: filters.assigned_node_id,
  keyword: filters.keyword,
  created_from: filters.created_from,
  created_to: filters.created_to,
  page: filters.page,
  page_size: filters.page_size,
  sort_by: filters.sort_by,
  sort_order: filters.sort_order,
}));

const tasksQuery = useQuery({
  queryKey: computed(() => ["tasks", taskParams.value]),
  queryFn: () => taskApi.list(taskParams.value),
});

const nodesQuery = useQuery({
  queryKey: ["task-nodes"],
  queryFn: () => nodeApi.list(),
});

const actionMutation = useMutation({
  mutationFn: async ({ task, action }: { task: TaskSummary; action: "start" | "stop" | "cancel" | "retry" | "delete" }) => {
    if (action === "start") return taskApi.start(task.id);
    if (action === "stop") return taskApi.stop(task.id);
    if (action === "cancel") return taskApi.cancel(task.id);
    if (action === "delete") return taskApi.delete(task.id);
    return taskApi.retry(task.id);
  },
  onSuccess: () => {
    queryClient.invalidateQueries({ queryKey: ["tasks"] });
  },
  onError: (error) => ElMessage.error(errorMessage(error)),
});

async function applyFilters() {
  await router.push({
    path: "/tasks",
    query: {
      status: filters.status || undefined,
      type: filters.type || undefined,
      assigned_node_id: filters.assigned_node_id || undefined,
      keyword: filters.keyword || undefined,
      created_from: filters.created_from || undefined,
      created_to: filters.created_to || undefined,
      page: filters.page > 1 ? String(filters.page) : undefined,
      page_size: filters.page_size !== 20 ? String(filters.page_size) : undefined,
      sort_by: filters.sort_by !== "created_at" ? filters.sort_by : undefined,
      sort_order: filters.sort_order !== "desc" ? filters.sort_order : undefined,
    },
  });
}

async function runAction(task: TaskSummary, action: "start" | "stop" | "cancel" | "retry" | "delete") {
  const actionLabel =
    action === "start"
      ? "启动"
      : action === "stop"
        ? "停止"
        : action === "cancel"
          ? "取消"
          : action === "retry"
            ? "重试"
            : "删除";
  const message =
    action === "delete"
      ? `确认删除任务 ${task.name} 吗？该操作会同时删除其尝试记录、事件、录像与产物索引。`
      : `确认对任务 ${task.name} 执行${actionLabel}吗？`;
  await ElMessageBox.confirm(message, "任务操作", { type: "warning" });
  await actionMutation.mutateAsync({ task, action });
  ElMessage.success(action === "delete" ? "任务已删除" : `已提交${actionLabel}请求`);
}

async function cloneTask(task: TaskSummary) {
  const cloned = await taskApi.clone(task.id, {});
  ElMessage.success(`已克隆任务 ${shortId(cloned.id)}`);
  await router.push(`/tasks/${cloned.id}`);
}

function rowActions(task: TaskSummary) {
  return {
    canStart: ["CREATED", "VALIDATING", "FAILED", "CANCELED"].includes(task.status),
    canStop: ["DISPATCHING", "STARTING", "RUNNING", "RECOVERING", "LOST"].includes(task.status),
    canCancel: ["CREATED", "VALIDATING", "QUEUED", "DISPATCHING", "STARTING", "RUNNING", "RECOVERING"].includes(task.status),
    canRetry: ["FAILED", "LOST"].includes(task.status),
    canClone: ["SUCCEEDED", "FAILED", "CANCELED", "LOST"].includes(task.status),
    canDelete: ["CREATED", "VALIDATING", "QUEUED", "SUCCEEDED", "FAILED", "CANCELED"].includes(task.status),
  };
}

function transcodeLabel(task: TaskSummary) {
  switch (task.transcode_mode) {
    case "none":
      return "不转码";
    case "adaptive":
      return "自适应";
    case "forced":
      return "转码";
    default:
      return "—";
  }
}

function transcodeTagType(task: TaskSummary) {
  switch (task.transcode_mode) {
    case "none":
      return "success";
    case "adaptive":
      return "warning";
    case "forced":
      return "danger";
    default:
      return "info";
  }
}
</script>

<template>
  <section class="page-grid">
    <PageHeader title="任务中心" description="统一管理任务列表、过滤条件、操作入口和创建跳转。">
      <el-button type="primary" @click="router.push('/tasks/new')">新建任务</el-button>
    </PageHeader>

    <div class="surface-card">
      <el-form label-position="top" inline>
        <el-form-item label="状态">
          <el-input v-model="filters.status" placeholder="RUNNING / FAILED" />
        </el-form-item>
        <el-form-item label="类型">
          <el-select v-model="filters.type" clearable style="width: 180px">
            <el-option v-for="item in TASK_TYPES" :key="item.value" :label="item.label" :value="item.value" />
          </el-select>
        </el-form-item>
        <el-form-item label="节点">
          <el-select v-model="filters.assigned_node_id" clearable filterable style="width: 220px">
            <el-option v-for="node in nodesQuery.data.value ?? []" :key="node.id" :label="node.node_name" :value="node.id" />
          </el-select>
        </el-form-item>
        <el-form-item label="关键字">
          <el-input v-model="filters.keyword" placeholder="任务名或 ID" />
        </el-form-item>
        <el-form-item label="创建时间起点">
          <el-date-picker
            v-model="filters.created_from"
            type="datetime"
            clearable
            format="YYYY-MM-DD HH:mm:ss"
            value-format="YYYY-MM-DDTHH:mm:ssZ"
            placeholder="选择开始时间"
          />
        </el-form-item>
        <el-form-item label="创建时间终点">
          <el-date-picker
            v-model="filters.created_to"
            type="datetime"
            clearable
            format="YYYY-MM-DD HH:mm:ss"
            value-format="YYYY-MM-DDTHH:mm:ssZ"
            placeholder="选择结束时间"
          />
        </el-form-item>
        <el-form-item>
          <el-button type="primary" @click="applyFilters">应用筛选</el-button>
        </el-form-item>
      </el-form>
    </div>

    <div class="surface-card">
      <div class="table-scroll">
        <el-table :data="tasksQuery.data.value?.items ?? []" v-loading="tasksQuery.isLoading.value">
        <el-table-column label="任务 ID" min-width="130">
          <template #default="{ row }">
            <el-link type="primary" @click="router.push(`/tasks/${row.id}`)">{{ shortId(row.id) }}</el-link>
          </template>
        </el-table-column>
        <el-table-column prop="name" label="名称" min-width="220" />
        <el-table-column label="类型" min-width="140">
          <template #default="{ row }">{{ taskTypeLabel(row.type) }}</template>
        </el-table-column>
        <el-table-column label="状态" min-width="120">
          <template #default="{ row }">
            <StatusTag :status="row.status" />
          </template>
        </el-table-column>
        <el-table-column label="转码" min-width="120">
          <template #default="{ row }">
            <el-tag :type="transcodeTagType(row)" effect="light" round>{{ transcodeLabel(row) }}</el-tag>
          </template>
        </el-table-column>
        <el-table-column prop="priority" label="优先级" min-width="100" />
        <el-table-column prop="created_by" label="创建人" min-width="140" />
        <el-table-column label="创建时间" min-width="180">
          <template #default="{ row }">{{ formatTime(row.created_at) }}</template>
        </el-table-column>
        <el-table-column label="操作" min-width="320" fixed="right">
          <template #default="{ row }">
            <div style="display: flex; gap: 8px; flex-wrap: wrap">
              <el-button link type="primary" @click="router.push(`/tasks/${row.id}`)">详情</el-button>
              <el-button v-if="rowActions(row).canStart" link @click="runAction(row, 'start')">启动</el-button>
              <el-button v-if="rowActions(row).canStop" link @click="runAction(row, 'stop')">停止</el-button>
              <el-button v-if="rowActions(row).canCancel" link @click="runAction(row, 'cancel')">取消</el-button>
              <el-button v-if="rowActions(row).canRetry" link @click="runAction(row, 'retry')">重试</el-button>
              <el-button v-if="rowActions(row).canClone" link @click="cloneTask(row)">克隆</el-button>
              <el-button v-if="rowActions(row).canDelete" link type="danger" @click="runAction(row, 'delete')">删除</el-button>
            </div>
          </template>
        </el-table-column>
        </el-table>
      </div>

      <div style="display: flex; justify-content: flex-end; margin-top: 16px">
        <el-pagination
          background
          layout="prev, pager, next, total"
          :current-page="filters.page"
          :page-size="filters.page_size"
          :total="tasksQuery.data.value?.total ?? 0"
          @current-change="(page: number) => { filters.page = page; applyFilters(); }"
        />
      </div>
    </div>
  </section>
</template>
