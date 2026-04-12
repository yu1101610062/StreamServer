<script setup lang="ts">
import { computed, nextTick, watch } from "vue";
import { useRoute } from "vue-router";
import { useQuery } from "@tanstack/vue-query";

import { AUTO_REFRESH_MS } from "@/shared/api/client";
import { artifactApi, recordApi, streamApi, taskApi } from "@/shared/api/resources";
import PageHeader from "@/shared/components/PageHeader.vue";
import StatusTag from "@/shared/components/StatusTag.vue";
import NodeOperationsSection from "@/shared/components/NodeOperationsSection.vue";
import { useSessionStore } from "@/stores/session";
import { formatTime } from "@/shared/utils/format";

const route = useRoute();
const sessionStore = useSessionStore();

const canReadTasks = computed(() => sessionStore.hasPermission("task_read"));
const canReadRecords = computed(() => sessionStore.hasPermission("record_read"));
const canReadNodes = computed(() => sessionStore.hasPermission("node_read"));

const tasksQuery = useQuery({
  queryKey: ["overview", "tasks"],
  enabled: canReadTasks,
  queryFn: () => taskApi.list({ page_size: 8, sort_by: "updated_at", sort_order: "desc" }),
  refetchInterval: AUTO_REFRESH_MS,
});

const streamsQuery = useQuery({
  queryKey: ["overview", "streams"],
  enabled: canReadTasks,
  queryFn: () => streamApi.list({}),
  refetchInterval: AUTO_REFRESH_MS,
});

const recordsQuery = useQuery({
  queryKey: ["overview", "records"],
  enabled: canReadRecords,
  queryFn: () => recordApi.list({ page_size: 1 }),
  refetchInterval: AUTO_REFRESH_MS,
});

const artifactsQuery = useQuery({
  queryKey: ["overview", "artifacts"],
  enabled: canReadRecords,
  queryFn: () => artifactApi.list({ page_size: 1 }),
  refetchInterval: AUTO_REFRESH_MS,
});

const metrics = computed(() => {
  const items: Array<{ label: string; value: string | number }> = [];
  if (canReadTasks.value) {
    items.push({ label: "任务总数", value: tasksQuery.data.value?.total ?? "—" });
    items.push({ label: "在线流", value: streamsQuery.data.value?.length ?? "—" });
  }
  if (canReadRecords.value) {
    items.push({ label: "录像记录", value: recordsQuery.data.value?.total ?? "—" });
    items.push({ label: "文件产物", value: artifactsQuery.data.value?.total ?? "—" });
  }
  items.push({ label: "当前身份", value: `${sessionStore.session?.subject ?? "—"} · ${sessionStore.session?.role ?? "—"}` });
  return items;
});

const capabilityCards = [
  {
    title: "流接入",
    description: "把 RTSP、RTMP、HLS、HTTP-TS、文件和 GB RTP 纳入平台，统一形成内部流。",
  },
  {
    title: "流桥接",
    description: "把实时源直接导出到文件或组播，适合向既有网络系统做旁路分发。",
  },
  {
    title: "录制与回看",
    description: "对内部流做 MP4 或 HLS 录制，并统一维护录像索引和 HTTP 回看地址。",
  },
  {
    title: "离线转码",
    description: "把文件和点播源转成产物文件，供归档、导出和二次分发使用。",
  },
];

const operationSteps = [
  {
    title: "先定目标",
    description: "先明确是接入内部流、桥接输出，还是做离线转码，再去新建任务页选择场景。",
  },
  {
    title: "检查规格",
    description: "创建前先做规格检查，确认系统如何解析输入源、调度方式和录制参数。",
  },
  {
    title: "观察运行态",
    description: "任务创建后到任务中心、流中心和本页节点区核对状态、流地址和节点负载。",
  },
];

async function maybeScrollToNodes() {
  if (route.query.focus !== "nodes" || !canReadNodes.value) {
    return;
  }
  await nextTick();
  document.getElementById("nodes-overview-section")?.scrollIntoView({ behavior: "smooth", block: "start" });
}

watch(
  () => route.query.focus,
  () => {
    void maybeScrollToNodes();
  },
  { immediate: true },
);
</script>

<template>
  <section class="page-grid">
    <PageHeader title="系统总览" description="在一页里看清平台能做什么、现在运行得怎么样，以及节点当前是否有足够余量承接任务。" />

    <div class="surface-card overview-hero">
      <div class="section-stack">
        <div>
          <div class="page-kicker">SYSTEM GUIDE</div>
          <h2 style="margin: 8px 0 10px">这是平台的控制面首页，不只是数据看板。</h2>
          <p class="subtle">
            这里会先说明平台能力边界，再给你当前任务、流、录像、转码和节点资源的实时概况。非技术同学可以先看功能介绍，再去新建任务页按引导完成配置。
          </p>
        </div>

        <div class="capability-card-grid">
          <div v-for="card in capabilityCards" :key="card.title" class="surface-panel capability-card">
            <strong>{{ card.title }}</strong>
            <p class="subtle">{{ card.description }}</p>
          </div>
        </div>
      </div>

      <div class="surface-panel" style="padding: 20px">
        <div class="page-kicker">HOW TO START</div>
        <h3 class="page-section-title" style="margin-top: 8px">建议使用路径</h3>
        <div class="overview-side-list">
          <div v-for="step in operationSteps" :key="step.title">
            <strong>{{ step.title }}</strong>
            <p class="subtle">{{ step.description }}</p>
          </div>
        </div>
      </div>
    </div>

    <div class="metric-grid">
      <div v-for="metric in metrics" :key="metric.label" class="surface-card metric-card">
        <div class="subtle">{{ metric.label }}</div>
        <strong>{{ metric.value }}</strong>
      </div>
    </div>

    <div v-if="canReadTasks" class="surface-card section-stack">
      <div>
        <h3 class="page-section-title">最近任务</h3>
        <p class="subtle">这里显示最近有更新的任务，适合快速确认创建、派发、运行和收尾状态。</p>
      </div>

      <div class="table-scroll">
        <el-table :data="tasksQuery.data.value?.items ?? []" v-loading="tasksQuery.isLoading.value">
          <el-table-column prop="name" label="任务名称" min-width="220" />
          <el-table-column prop="type" label="类型" min-width="140" />
          <el-table-column label="状态" min-width="120">
            <template #default="{ row }">
              <StatusTag :status="row.status" />
            </template>
          </el-table-column>
          <el-table-column label="更新时间" min-width="180">
            <template #default="{ row }">{{ formatTime(row.updated_at) }}</template>
          </el-table-column>
        </el-table>
      </div>
    </div>

    <NodeOperationsSection
      v-if="canReadNodes"
      anchor-id="nodes-overview-section"
      title="节点与容量总览"
      description="这里已经合并了原节点中心的能力：既能看当前健康和容量，也能下钻到单节点心跳与能力矩阵。"
    />
  </section>
</template>
