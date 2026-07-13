<script setup lang="ts">
import { computed, ref } from "vue";
import { useQuery } from "@tanstack/vue-query";

import type { NodeSummary, RuntimeSlotLoad } from "@/shared/api/types";
import { nodeApi } from "@/shared/api/resources";
import { formatPercent, formatTime } from "@/shared/utils/format";

const props = withDefaults(
  defineProps<{
    title?: string;
    description?: string;
    anchorId?: string;
  }>(),
  {
    title: "节点与容量",
    description: "查看节点健康、能力矩阵、实时负载和最近心跳。",
    anchorId: "",
  },
);

const selectedNodeId = ref("");

const nodesQuery = useQuery({
  queryKey: ["nodes"],
  queryFn: () => nodeApi.list(),
});

const selectedNode = computed(
  () => (nodesQuery.data.value ?? []).find((node) => node.id === selectedNodeId.value) ?? null,
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

const nodeMetrics = computed(() => {
  const nodes = nodesQuery.data.value ?? [];
  const total = nodes.length;
  const healthy = nodes.filter((node) => node.healthy).length;
  const avgCpu =
    total > 0 ? nodes.reduce((sum, node) => sum + (node.cpu_percent ?? 0), 0) / total : null;
  const avgMem =
    total > 0 ? nodes.reduce((sum, node) => sum + (node.mem_percent ?? 0), 0) / total : null;
  const runningTasks = nodes.reduce((sum, node) => sum + (node.running_tasks ?? 0), 0);
  return [
    { label: "节点总数", value: total || "—" },
    { label: "健康节点", value: healthy || 0 },
    { label: "平均 CPU", value: formatPercent(avgCpu) },
    { label: "平均内存", value: formatPercent(avgMem) },
    { label: "运行任务", value: runningTasks || 0 },
  ];
});

function openNode(node: NodeSummary) {
  selectedNodeId.value = node.id;
}

function capabilityCount(values: unknown[] | undefined) {
  return values?.length ?? 0;
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

function slotModeLabel(sourceMode: string) {
  return sourceMode === "live" ? "直播" : sourceMode === "vod" ? "点播" : sourceMode;
}

function slotLoadText(load: RuntimeSlotLoad) {
  const maxSlots = load.max_runtime_slots === 0 ? "不限" : String(load.max_runtime_slots);
  const occupied =
    load.running_tasks + load.starting_tasks + load.stopping_tasks + load.orphaned_tasks;
  return `${slotModeLabel(load.source_mode)} ${occupied}/${maxSlots} ${formatPercent((load.slot_usage ?? 0) * 100)}`;
}

function slotLoadSummary(loads?: RuntimeSlotLoad[] | null) {
  const values = loads ?? [];
  return values.length ? values.map(slotLoadText).join(" · ") : "—";
}
</script>

<template>
  <section :id="anchorId || undefined" class="page-grid">
    <div class="surface-card section-stack">
      <div class="page-header page-header--embedded">
        <div>
          <div class="page-kicker">NODE CONTROL</div>
          <h2>{{ title }}</h2>
          <p>{{ description }}</p>
        </div>
      </div>

      <div class="metric-grid">
        <div v-for="metric in nodeMetrics" :key="metric.label" class="surface-panel metric-card">
          <div class="subtle">{{ metric.label }}</div>
          <strong>{{ metric.value }}</strong>
        </div>
      </div>

      <div class="node-card-grid">
        <div v-for="node in nodesQuery.data.value ?? []" :key="node.id" class="surface-panel node-card">
          <div class="table-actions" style="justify-content: space-between; margin-bottom: 14px">
            <div>
              <strong>{{ node.node_name }}</strong>
              <div class="subtle">{{ node.hostname }} · {{ node.network_mode }}</div>
            </div>
            <el-tag :type="nodeHealthTagType(node)" effect="light" round>{{ nodeHealthLabel(node) }}</el-tag>
          </div>

          <div class="progress-stack">
            <div>
              <div class="progress-label">
                <span>CPU</span>
                <strong>{{ formatPercent(node.cpu_percent) }}</strong>
              </div>
              <el-progress :percentage="Math.round(node.cpu_percent ?? 0)" :stroke-width="10" :show-text="false" />
            </div>
            <div>
              <div class="progress-label">
                <span>内存</span>
                <strong>{{ formatPercent(node.mem_percent) }}</strong>
              </div>
              <el-progress :percentage="Math.round(node.mem_percent ?? 0)" :stroke-width="10" :show-text="false" />
            </div>
          </div>

          <div class="node-stat-list">
            <div>
              <span class="subtle">运行任务</span>
              <strong>{{ node.running_tasks ?? 0 }}</strong>
            </div>
            <div>
              <span class="subtle">槽位占用</span>
              <strong>{{ slotLoadSummary(node.runtime_slot_loads) }}</strong>
            </div>
            <div>
              <span class="subtle">ZLM / FFmpeg</span>
              <strong>{{ node.zlm_alive ? "正常" : "异常" }} / {{ node.ffmpeg_alive ? "正常" : "异常" }}</strong>
            </div>
            <div>
              <span class="subtle">最近上报</span>
              <strong>{{ formatTime(node.last_seen_at) }}</strong>
            </div>
          </div>

          <div class="tag-cloud">
            <el-tag v-for="label in displayNodeLabels(node).slice(0, 6)" :key="label" round>{{ label }}</el-tag>
            <span v-if="displayNodeLabels(node).length > 6" class="subtle">
              +{{ displayNodeLabels(node).length - 6 }} 个标签
            </span>
          </div>

          <div class="capability-summary">
            <span>协议 {{ capabilityCount(node.ffmpeg_protocols) }}</span>
            <span>格式 {{ capabilityCount(node.ffmpeg_formats) }}</span>
            <span>编码器 {{ capabilityCount(node.ffmpeg_encoders) }}</span>
            <span>GPU {{ capabilityCount(node.gpu) }}</span>
          </div>

          <el-button style="margin-top: 14px" @click="openNode(node)">查看节点详情</el-button>
        </div>
      </div>
    </div>

    <div class="surface-card">
      <div class="section-stack" style="gap: 8px; margin-bottom: 16px">
        <h3 class="page-section-title">节点明细表</h3>
        <p class="subtle">适合快速横向比较节点健康、最近上报时间和运行能力。</p>
      </div>

      <div class="table-scroll">
        <el-table :data="nodesQuery.data.value ?? []" v-loading="nodesQuery.isLoading.value">
          <el-table-column prop="node_name" label="节点" min-width="160" />
          <el-table-column prop="hostname" label="主机名" min-width="180" />
          <el-table-column label="网络模式" min-width="120">
            <template #default="{ row }">{{ row.network_mode }}</template>
          </el-table-column>
          <el-table-column label="CPU / 内存 / 磁盘" min-width="220">
            <template #default="{ row }">
              {{ formatPercent(row.cpu_percent) }} / {{ formatPercent(row.mem_percent) }} / {{ formatPercent(row.disk_percent) }}
            </template>
          </el-table-column>
          <el-table-column label="运行任务 / 槽位" min-width="150">
            <template #default="{ row }">{{ row.running_tasks ?? 0 }} / {{ slotLoadSummary(row.runtime_slot_loads) }}</template>
          </el-table-column>
          <el-table-column label="ZLM / FFmpeg" min-width="160">
            <template #default="{ row }">
              {{ row.zlm_alive ? "ZLM 正常" : "ZLM 异常" }} / {{ row.ffmpeg_alive ? "FFmpeg 正常" : "FFmpeg 异常" }}
            </template>
          </el-table-column>
          <el-table-column label="最近上报" min-width="180">
            <template #default="{ row }">{{ formatTime(row.last_seen_at) }}</template>
          </el-table-column>
          <el-table-column label="操作" min-width="120" fixed="right">
            <template #default="{ row }">
              <el-button link type="primary" @click="openNode(row)">详情</el-button>
            </template>
          </el-table-column>
        </el-table>
      </div>
    </div>

    <el-drawer v-model="drawerOpen" :with-header="false" size="760px" destroy-on-close>
      <div v-if="selectedNode" class="page-grid">
        <div class="surface-card section-stack">
          <div class="page-header page-header--embedded">
            <div>
              <div class="page-kicker">NODE DETAIL</div>
              <h2>{{ selectedNode.node_name }}</h2>
              <p>节点能力、最近心跳与实时负载。</p>
            </div>
            <el-tag :type="nodeHealthTagType(selectedNode)" effect="light" round>
              {{ nodeHealthLabel(selectedNode) }}
            </el-tag>
          </div>

          <el-descriptions :column="2" border>
            <el-descriptions-item label="节点 ID">{{ selectedNode.id }}</el-descriptions-item>
            <el-descriptions-item label="主机名">{{ selectedNode.hostname }}</el-descriptions-item>
            <el-descriptions-item label="流地址">{{ selectedNode.agent_stream_addr }}</el-descriptions-item>
            <el-descriptions-item label="ZLM 管理">经认证 Agent 控制流转发</el-descriptions-item>
            <el-descriptions-item label="网络模式">{{ selectedNode.network_mode }}</el-descriptions-item>
            <el-descriptions-item label="最近上报">{{ formatTime(selectedNode.last_seen_at) }}</el-descriptions-item>
            <el-descriptions-item label="能力采集时间">{{ formatTime(selectedNode.capability_captured_at) }}</el-descriptions-item>
            <el-descriptions-item label="标签">{{ displayNodeLabels(selectedNode).join(", ") || "—" }}</el-descriptions-item>
          </el-descriptions>
        </div>

        <div class="surface-card section-stack">
          <h3 class="page-section-title">能力矩阵</h3>
          <el-descriptions :column="1" border>
            <el-descriptions-item label="网卡">{{ selectedNode.interfaces.join(", ") || "—" }}</el-descriptions-item>
            <el-descriptions-item label="FFmpeg 协议">{{ selectedNode.ffmpeg_protocols.join(", ") || "—" }}</el-descriptions-item>
            <el-descriptions-item label="FFmpeg 格式">{{ selectedNode.ffmpeg_formats.join(", ") || "—" }}</el-descriptions-item>
            <el-descriptions-item label="编码器">{{ selectedNode.ffmpeg_encoders.join(", ") || "—" }}</el-descriptions-item>
            <el-descriptions-item label="解码器">{{ selectedNode.ffmpeg_decoders.join(", ") || "—" }}</el-descriptions-item>
            <el-descriptions-item label="GPU">{{ selectedNode.gpu.join(", ") || "—" }}</el-descriptions-item>
            <el-descriptions-item label="ZLM API 列表">{{ selectedNode.zlm_api_list.join(", ") || "—" }}</el-descriptions-item>
          </el-descriptions>
        </div>

        <div class="surface-card section-stack">
          <div>
            <h3 class="page-section-title">最近心跳</h3>
            <p class="subtle">用于判断节点负载变化、心跳稳定性和运行任务数量是否异常。</p>
          </div>

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
              <el-table-column label="槽位" min-width="220">
                <template #default="{ row }">{{ slotLoadSummary(row.runtime_slot_loads) }}</template>
              </el-table-column>
              <el-table-column label="ZLM / FFmpeg" min-width="160">
                <template #default="{ row }">{{ row.zlm_alive ? "正常" : "异常" }} / {{ row.ffmpeg_alive ? "正常" : "异常" }}</template>
              </el-table-column>
            </el-table>
          </div>
        </div>
      </div>
    </el-drawer>
  </section>
</template>
