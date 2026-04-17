<script setup lang="ts">
import { computed, reactive, watch } from "vue";
import { useRoute, useRouter } from "vue-router";
import { useQuery } from "@tanstack/vue-query";
import { ElMessage } from "element-plus";

import { artifactApi } from "@/shared/api/resources";
import type { OptionItem } from "@/shared/labels";
import PageHeader from "@/shared/components/PageHeader.vue";
import { copyText } from "@/shared/utils/clipboard";
import { formatBytes, formatTime, shortId } from "@/shared/utils/format";

const route = useRoute();
const router = useRouter();

const filters = reactive({
  artifact_kind: String(route.query.artifact_kind ?? ""),
  task_id: String(route.query.task_id ?? ""),
  date_from: String(route.query.date_from ?? ""),
  date_to: String(route.query.date_to ?? ""),
  page: Number(route.query.page ?? 1),
  page_size: Number(route.query.page_size ?? 20),
});

watch(
  () => route.query,
  (query) => {
    filters.artifact_kind = String(query.artifact_kind ?? "");
    filters.task_id = String(query.task_id ?? "");
    filters.date_from = String(query.date_from ?? "");
    filters.date_to = String(query.date_to ?? "");
    filters.page = Number(query.page ?? 1);
    filters.page_size = Number(query.page_size ?? 20);
  },
);

const queryParams = computed(() => ({ ...filters }));

const artifactsQuery = useQuery({
  queryKey: computed(() => ["artifacts", queryParams.value]),
  queryFn: () => artifactApi.list(queryParams.value),
});

const artifactKindOptions: OptionItem[] = [
  { value: "", label: "全部文件产物" },
  { value: "transcode_output", label: "转码输出" },
  { value: "bridge_output", label: "桥接输出" },
  { value: "stream_ingest_record", label: "流接入快录" },
];

function artifactKindLabel(value: string) {
  if (value === "bridge_output") return "桥接输出";
  if (value === "transcode_output") return "转码输出";
  if (value === "stream_ingest_record") return "流接入快录";
  return "—";
}

async function applyFilters() {
  await router.push({
    path: "/file-artifacts",
    query: {
      artifact_kind: filters.artifact_kind || undefined,
      task_id: filters.task_id || undefined,
      date_from: filters.date_from || undefined,
      date_to: filters.date_to || undefined,
      page: filters.page > 1 ? String(filters.page) : undefined,
      page_size: filters.page_size !== 20 ? String(filters.page_size) : undefined,
    },
  });
}

async function resetFilters() {
  filters.artifact_kind = "";
  filters.task_id = "";
  filters.date_from = "";
  filters.date_to = "";
  filters.page = 1;
  filters.page_size = 20;
  await applyFilters();
}
</script>

<template>
  <section class="page-grid">
    <PageHeader title="文件产物" description="查看桥接输出、转码输出和流接入快录文件的路径与节点 HTTP 地址。" />

    <div class="surface-card">
      <el-form label-position="top" inline>
        <el-form-item label="产物类型">
          <el-select v-model="filters.artifact_kind" style="width: 180px" clearable placeholder="全部文件产物">
            <el-option v-for="item in artifactKindOptions" :key="item.value || '__all__'" :label="item.label" :value="item.value" />
          </el-select>
        </el-form-item>
        <el-form-item label="任务 ID">
          <el-input v-model="filters.task_id" placeholder="任务 UUID" />
        </el-form-item>
        <el-form-item label="开始时间">
          <el-date-picker
            v-model="filters.date_from"
            type="datetime"
            clearable
            format="YYYY-MM-DD HH:mm:ss"
            value-format="YYYY-MM-DDTHH:mm:ssZ"
            placeholder="选择开始时间"
          />
        </el-form-item>
        <el-form-item label="结束时间">
          <el-date-picker
            v-model="filters.date_to"
            type="datetime"
            clearable
            format="YYYY-MM-DD HH:mm:ss"
            value-format="YYYY-MM-DDTHH:mm:ssZ"
            placeholder="选择结束时间"
          />
        </el-form-item>
        <el-form-item>
          <el-button type="primary" @click="applyFilters">应用筛选</el-button>
          <el-button @click="resetFilters">重置</el-button>
        </el-form-item>
      </el-form>
    </div>

    <div class="surface-card">
      <div class="table-scroll">
        <el-table :data="artifactsQuery.data.value?.items ?? []" v-loading="artifactsQuery.isLoading.value">
        <el-table-column label="产物 ID" min-width="140">
          <template #default="{ row }">{{ shortId(row.id) }}</template>
        </el-table-column>
        <el-table-column label="产物类型" min-width="120">
          <template #default="{ row }">{{ artifactKindLabel(row.artifact_kind) }}</template>
        </el-table-column>
        <el-table-column label="任务" min-width="220">
          <template #default="{ row }">
            <div>
              <el-link type="primary" @click="router.push(`/tasks/${row.task_id}`)">{{ row.task_name || shortId(row.task_id) }}</el-link>
            </div>
            <div class="subtle">{{ shortId(row.task_id) }}</div>
          </template>
        </el-table-column>
        <el-table-column prop="node_id" label="节点" min-width="180" />
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
      </div>

      <div style="display: flex; justify-content: flex-end; margin-top: 16px">
        <el-pagination
          background
          layout="prev, pager, next, total"
          :current-page="filters.page"
          :page-size="filters.page_size"
          :total="artifactsQuery.data.value?.total ?? 0"
          @current-change="(page: number) => { filters.page = page; applyFilters(); }"
        />
      </div>
    </div>
  </section>
</template>
