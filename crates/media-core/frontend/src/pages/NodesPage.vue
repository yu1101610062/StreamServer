<script setup lang="ts">
import { computed, ref } from "vue";
import { useQuery } from "@tanstack/vue-query";

import { nodeApi } from "@/shared/api/resources";
import type { NodeSummary } from "@/shared/api/types";
import PageHeader from "@/shared/components/PageHeader.vue";
import { formatPercent, formatTime } from "@/shared/utils/format";

const selectedNodeId = ref("");

const nodesQuery = useQuery({
  queryKey: ["nodes"],
  queryFn: () => nodeApi.list(),
});

const selectedNode = computed(() =>
  (nodesQuery.data.value ?? []).find((node) => node.id === selectedNodeId.value) ?? null,
);

const drawerOpen = computed({
  get: () => Boolean(selectedNodeId.value),
  set: (value: boolean) => {
    if (!value) {
      selectedNodeId.value = "";
    }
  },
});

const heartbeatsQuery = useQuery({
  queryKey: computed(() => ["node-heartbeats", selectedNodeId.value]),
  enabled: computed(() => Boolean(selectedNodeId.value)),
  queryFn: () => nodeApi.heartbeats(selectedNodeId.value, 24),
});

function openNode(node: NodeSummary) {
  selectedNodeId.value = node.id;
}

function displayNodeLabels(node: NodeSummary) {
  return node.labels.filter((label) => label.trim() && label.trim().toLowerCase() !== "offline");
}

function nodeHealthTagType(node: NodeSummary) {
  return node.healthy ? "success" : "danger";
}

function nodeHealthLabel(node: NodeSummary) {
  return node.healthy ? "正常" : "异常";
}
</script>

<template>
  <section class="page-grid">
    <PageHeader title="节点中心" description="查看节点健康、能力矩阵、实时负载和最近心跳。" />

    <div class="metric-grid">
      <div v-for="node in nodesQuery.data.value ?? []" :key="node.id" class="surface-card metric-card">
        <div class="table-actions" style="justify-content: space-between; margin-bottom: 12px">
          <strong>{{ node.node_name }}</strong>
          <el-tag :type="nodeHealthTagType(node)" effect="light" round>{{ nodeHealthLabel(node) }}</el-tag>
        </div>
        <div class="subtle">{{ node.hostname }}</div>
        <div class="subtle">CPU {{ formatPercent(node.cpu_percent) }} · 内存 {{ formatPercent(node.mem_percent) }}</div>
        <div class="subtle">运行任务 {{ node.running_tasks ?? 0 }} · 槽位 {{ node.slot_usage ?? 0 }}</div>
        <el-button style="margin-top: 14px" @click="openNode(node)">查看详情</el-button>
      </div>
    </div>

    <div class="surface-card">
      <div class="table-scroll">
        <el-table :data="nodesQuery.data.value ?? []" v-loading="nodesQuery.isLoading.value">
        <el-table-column prop="node_name" label="节点" min-width="160" />
        <el-table-column prop="hostname" label="主机名" min-width="180" />
        <el-table-column label="网络模式" min-width="120">
          <template #default="{ row }">{{ row.network_mode }}</template>
        </el-table-column>
        <el-table-column label="最近上报" min-width="180">
          <template #default="{ row }">{{ formatTime(row.last_seen_at) }}</template>
        </el-table-column>
        <el-table-column label="ZLM / FFmpeg" min-width="160">
          <template #default="{ row }">{{ row.zlm_alive ? "ZLM 正常" : "ZLM 异常" }} / {{ row.ffmpeg_alive ? "FFmpeg 正常" : "FFmpeg 异常" }}</template>
        </el-table-column>
          <el-table-column label="操作" min-width="120" fixed="right">
            <template #default="{ row }">
              <el-button link type="primary" @click="openNode(row)">详情</el-button>
            </template>
          </el-table-column>
        </el-table>
      </div>
    </div>

    <el-drawer v-model="drawerOpen" :with-header="false" size="720px" destroy-on-close>
      <div v-if="selectedNode" class="page-grid">
        <PageHeader :title="selectedNode.node_name" description="节点能力、最近心跳与实时负载。" />

        <div class="surface-card">
          <el-descriptions :column="2" border>
            <el-descriptions-item label="节点 ID">{{ selectedNode.id }}</el-descriptions-item>
            <el-descriptions-item label="主机名">{{ selectedNode.hostname }}</el-descriptions-item>
            <el-descriptions-item label="流地址">{{ selectedNode.agent_stream_addr }}</el-descriptions-item>
            <el-descriptions-item label="ZLM API">{{ selectedNode.zlm_api_base }}</el-descriptions-item>
            <el-descriptions-item label="网络模式">{{ selectedNode.network_mode }}</el-descriptions-item>
            <el-descriptions-item label="最近上报">{{ formatTime(selectedNode.last_seen_at) }}</el-descriptions-item>
          </el-descriptions>
        </div>

        <div class="surface-card">
          <h3 class="page-section-title">能力矩阵</h3>
          <el-descriptions :column="1" border>
            <el-descriptions-item label="标签">{{ displayNodeLabels(selectedNode).join(", ") || "—" }}</el-descriptions-item>
            <el-descriptions-item label="网卡">{{ selectedNode.interfaces.join(", ") || "—" }}</el-descriptions-item>
            <el-descriptions-item label="FFmpeg 协议">{{ selectedNode.ffmpeg_protocols.join(", ") || "—" }}</el-descriptions-item>
            <el-descriptions-item label="FFmpeg 格式">{{ selectedNode.ffmpeg_formats.join(", ") || "—" }}</el-descriptions-item>
            <el-descriptions-item label="编码器">{{ selectedNode.ffmpeg_encoders.join(", ") || "—" }}</el-descriptions-item>
            <el-descriptions-item label="解码器">{{ selectedNode.ffmpeg_decoders.join(", ") || "—" }}</el-descriptions-item>
            <el-descriptions-item label="GPU">{{ selectedNode.gpu.join(", ") || "—" }}</el-descriptions-item>
          </el-descriptions>
        </div>

        <div class="surface-card">
          <h3 class="page-section-title">最近心跳</h3>
          <div class="table-scroll">
            <el-table :data="heartbeatsQuery.data.value ?? []" v-loading="heartbeatsQuery.isLoading.value">
              <el-table-column label="时间" min-width="180">
                <template #default="{ row }">{{ formatTime(row.received_at) }}</template>
              </el-table-column>
              <el-table-column label="CPU" min-width="100">
                <template #default="{ row }">{{ formatPercent(row.cpu_percent) }}</template>
              </el-table-column>
              <el-table-column label="内存" min-width="100">
                <template #default="{ row }">{{ formatPercent(row.mem_percent) }}</template>
              </el-table-column>
              <el-table-column label="磁盘" min-width="100">
                <template #default="{ row }">{{ formatPercent(row.disk_percent) }}</template>
              </el-table-column>
              <el-table-column prop="running_tasks" label="运行任务" min-width="100" />
              <el-table-column prop="slot_usage" label="槽位" min-width="100" />
            </el-table>
          </div>
        </div>
      </div>
    </el-drawer>
  </section>
</template>
