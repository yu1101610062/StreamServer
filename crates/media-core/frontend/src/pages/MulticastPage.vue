<script setup lang="ts">
import { computed } from "vue";
import { useQuery } from "@tanstack/vue-query";

import { streamApi, taskApi } from "@/shared/api/resources";
import type { TaskDetail, TaskSummary, UnknownJson } from "@/shared/api/types";
import PageHeader from "@/shared/components/PageHeader.vue";
import StatusTag from "@/shared/components/StatusTag.vue";
import { inputKindLabel, publishKindLabel } from "@/shared/labels";
import { deriveLastIssue, formatBitrateKbps, shortId } from "@/shared/utils/format";

interface MulticastRow {
  task: TaskSummary;
  detail: TaskDetail | null;
  mode: string;
  group: string;
  port: string;
  interfaceIp: string;
  ttl: string;
  bitrate: string;
  lastError: string;
}

const tasksQuery = useQuery({
  queryKey: ["multicast", "tasks"],
  queryFn: () =>
    taskApi.list({
      type: "stream_bridge",
      page: 1,
      page_size: 100,
      sort_by: "created_at",
      sort_order: "desc",
    }),
});

const detailsQuery = useQuery({
  queryKey: computed(() => ["multicast", "details", (tasksQuery.data.value?.items ?? []).map((item) => item.id)]),
  enabled: computed(() => (tasksQuery.data.value?.items?.length ?? 0) > 0),
  queryFn: async () => {
    const items = tasksQuery.data.value?.items ?? [];
    const details = await Promise.all(items.map((item) => taskApi.detail(item.id).catch(() => null)));
    return new Map(items.map((item, index) => [item.id, details[index]]));
  },
});

const streamsQuery = useQuery({
  queryKey: ["multicast", "streams"],
  queryFn: () => streamApi.list({}),
});

const rows = computed<MulticastRow[]>(() => {
  const detailMap = detailsQuery.data.value ?? new Map<string, TaskDetail | null>();
  const streamMap = new Map(
    (streamsQuery.data.value ?? []).map((stream) => [stream.task_id, stream]),
  );

  return (tasksQuery.data.value?.items ?? [])
    .map((task) => {
      const detail = detailMap.get(task.id) ?? null;
      const spec = (detail?.resolved_spec ?? {}) as UnknownJson;
      const input = ((spec.input ?? {}) as UnknownJson) ?? {};
      const publish = ((spec.publish ?? {}) as UnknownJson) ?? {};
      const kind = String(publish.kind ?? "");
      if (!["udp_mpegts_multicast", "rtp_multicast"].includes(kind)) {
        return null;
      }
      const runtime = streamMap.get(task.id);
      return {
        task,
        detail,
        mode: `${inputKindLabel(String(input.kind ?? ""))} -> ${publishKindLabel(kind)}`,
        group: String(publish.group ?? "—"),
        port: publish.port != null ? String(publish.port) : "—",
        interfaceIp: String(publish.interface_ip ?? publish.interface_name ?? "—"),
        ttl: publish.ttl != null ? String(publish.ttl) : "—",
        bitrate: formatBitrateKbps(runtime?.bitrate_kbps ?? null),
        lastError: deriveLastIssue(detail?.recent_events ?? []),
      };
    })
    .filter((item): item is MulticastRow => Boolean(item));
});
</script>

<template>
  <section class="page-grid">
    <PageHeader title="组播中心" description="集中查看组播桥接任务、绑定地址、TTL、最近码率和错误摘要。" />

    <div class="surface-card">
      <div class="section-stack" style="gap: 8px; margin-bottom: 16px">
        <h3 class="page-section-title">组播桥接任务</h3>
        <p class="subtle">这里只展示输出目标为 UDP MPEGTS 组播或 RTP 组播的 stream_bridge 任务。</p>
      </div>

      <div class="table-scroll">
        <el-table :data="rows" v-loading="tasksQuery.isLoading.value || detailsQuery.isLoading.value">
        <el-table-column label="任务" min-width="220">
          <template #default="{ row }">
            <div>
              <el-link type="primary" :href="`/tasks/${row.task.id}`">{{ row.task.name || shortId(row.task.id) }}</el-link>
            </div>
            <div class="subtle">{{ shortId(row.task.id) }}</div>
          </template>
        </el-table-column>
        <el-table-column prop="mode" label="模式" min-width="220" />
        <el-table-column prop="group" label="组播地址" min-width="180" />
        <el-table-column prop="port" label="端口" min-width="100" />
        <el-table-column prop="interfaceIp" label="绑定地址" min-width="160" />
        <el-table-column prop="ttl" label="TTL" min-width="100" />
        <el-table-column prop="bitrate" label="最近码率" min-width="120" />
        <el-table-column label="状态" min-width="120">
          <template #default="{ row }">
            <StatusTag :status="row.task.status" />
          </template>
        </el-table-column>
        <el-table-column prop="lastError" label="最近错误" min-width="320" />
        </el-table>
      </div>
    </div>
  </section>
</template>
