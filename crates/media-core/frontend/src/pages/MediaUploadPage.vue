<script setup lang="ts">
import { computed, ref } from "vue";
import { useQuery } from "@tanstack/vue-query";
import { ElMessage, ElMessageBox } from "element-plus";
import { CopyDocument, Delete, Refresh, Upload, UploadFilled } from "@element-plus/icons-vue";

import { mediaUploadApi, nodeApi } from "@/shared/api/resources";
import type { MediaUploadAssetSummary, NodeSummary, UploadMediaResponse } from "@/shared/api/types";
import MediaLink from "@/shared/components/MediaLink.vue";
import PageHeader from "@/shared/components/PageHeader.vue";
import { copyText } from "@/shared/utils/clipboard";
import { errorMessage, formatBytes, formatPercent, formatTime, shortId } from "@/shared/utils/format";

const allowedExtensions = ["mp4", "mov", "m4v", "mkv", "webm", "ts", "m2ts", "mts", "flv"];
const accept = allowedExtensions.map((extension) => `.${extension}`).join(",");

const fileInput = ref<HTMLInputElement | null>(null);
const selectedFile = ref<File | null>(null);
const uploadResult = ref<UploadMediaResponse | null>(null);
const uploadError = ref("");
const uploading = ref(false);
const dragging = ref(false);
const elapsedMs = ref<number | null>(null);
const uploadProgress = ref(0);
const uploadPhase = ref<"idle" | "uploading" | "processing" | "done">("idle");
const targetMode = ref<"auto" | "node" | "labels">("auto");
const selectedNodeId = ref("");
const requiredLabelsText = ref("");
const assetKeyword = ref("");
const assetNodeId = ref("");
const assetStatus = ref<"active" | "deleted" | "all">("active");

const nodesQuery = useQuery({
  queryKey: ["media-upload", "nodes"],
  queryFn: () => nodeApi.list(),
});

const assetsQuery = useQuery({
  queryKey: ["media-upload", "assets"],
  queryFn: () =>
    mediaUploadApi.list({
      status: assetStatus.value,
      node_id: assetNodeId.value,
      keyword: assetKeyword.value.trim(),
      page_size: 100,
    }),
});

const uploadReadyNodes = computed(() =>
  (nodesQuery.data.value ?? []).filter((node) => nodeUploadReady(node)),
);

const uploadedNodeId = computed(() => {
  const sourceUrl = uploadResult.value?.sourceUrl.trim().replace(/^\/+/, "") ?? "";
  const parts = sourceUrl.split("/");
  return parts[0] === "uploads" ? parts[1] ?? "" : "";
});

const uploadedNode = computed(() =>
  (nodesQuery.data.value ?? []).find((node) => node.id === uploadedNodeId.value) ?? null,
);

const selectedExtension = computed(() => extensionOf(selectedFile.value?.name ?? ""));
const uploadAssets = computed(() => assetsQuery.data.value?.items ?? []);
const uploadProgressStatus = computed(() => (uploadPhase.value === "done" ? "success" : undefined));
const uploadProgressLabel = computed(() => {
  if (uploadPhase.value === "uploading") {
    return uploadProgress.value > 0 ? `上传中 ${uploadProgress.value}%` : "上传中";
  }
  if (uploadPhase.value === "processing") {
    return "上传已发送，后端处理中";
  }
  if (uploadPhase.value === "done") {
    return "上传完成";
  }
  return "";
});

function extensionOf(fileName: string) {
  const extension = fileName.split(".").pop()?.trim().toLowerCase() ?? "";
  return extension === fileName ? "" : extension;
}

function uploadCreatedAt(value?: number | null) {
  if (!value) {
    return "—";
  }
  return new Date(value).toLocaleString("zh-CN", { hour12: false });
}

function elapsedLabel(value?: number | null) {
  if (value === undefined || value === null) {
    return "—";
  }
  return `${(value / 1000).toFixed(value >= 10_000 ? 1 : 2)}s`;
}

function nodeUploadReady(node: NodeSummary) {
  return (
    node.healthy &&
    node.control_connected &&
    node.connected !== false &&
    node.ffmpeg_alive !== false &&
    Boolean(node.agent_http_base_url?.trim())
  );
}

function nodeLabels(node: NodeSummary) {
  return node.labels.filter((label) => label.trim());
}

function uploadParams() {
  if (targetMode.value === "node") {
    return { nodeId: selectedNodeId.value || undefined };
  }
  if (targetMode.value === "labels") {
    return { requiredLabels: requiredLabelsText.value.trim() || undefined };
  }
  return {};
}

function chooseFile() {
  fileInput.value?.click();
}

function onFileInputChange(event: Event) {
  const input = event.target as HTMLInputElement;
  setSelectedFile(input.files?.[0] ?? null);
  input.value = "";
}

function onDrop(event: DragEvent) {
  dragging.value = false;
  setSelectedFile(event.dataTransfer?.files?.[0] ?? null);
}

function setSelectedFile(file: File | null) {
  uploadError.value = "";
  elapsedMs.value = null;
  uploadProgress.value = 0;
  uploadPhase.value = "idle";
  if (!file) {
    selectedFile.value = null;
    return;
  }

  const extension = extensionOf(file.name);
  if (!allowedExtensions.includes(extension)) {
    ElMessage.warning(`不支持的文件扩展名：${extension || "无扩展名"}`);
    return;
  }
  selectedFile.value = file;
}

function clearSelection() {
  selectedFile.value = null;
  uploadError.value = "";
  elapsedMs.value = null;
  uploadProgress.value = 0;
  uploadPhase.value = "idle";
}

async function submitUpload() {
  if (!selectedFile.value) {
    ElMessage.warning("请选择要上传的媒资文件");
    return;
  }
  if (targetMode.value === "node" && !selectedNodeId.value) {
    ElMessage.warning("请选择上传节点");
    return;
  }
  if (targetMode.value === "labels" && !requiredLabelsText.value.trim()) {
    ElMessage.warning("请输入节点标签");
    return;
  }

  uploading.value = true;
  uploadError.value = "";
  uploadResult.value = null;
  uploadProgress.value = 0;
  uploadPhase.value = "uploading";
  const startedAt = performance.now();
  try {
    uploadResult.value = await mediaUploadApi.upload(selectedFile.value, uploadParams(), {
      onProgress: ({ percent }) => {
        if (percent === null) {
          return;
        }
        uploadProgress.value = percent;
        if (percent >= 100) {
          uploadPhase.value = "processing";
        }
      },
    });
    uploadProgress.value = 100;
    uploadPhase.value = "done";
    elapsedMs.value = performance.now() - startedAt;
    ElMessage.success("媒资上传完成");
    await assetsQuery.refetch();
  } catch (error) {
    elapsedMs.value = performance.now() - startedAt;
    uploadError.value = errorMessage(error);
    uploadPhase.value = "idle";
    ElMessage.error(uploadError.value);
  } finally {
    uploading.value = false;
  }
}

async function deleteAsset(asset: MediaUploadAssetSummary) {
  let deleteFile = false;
  try {
    await ElMessageBox.confirm(
      "同步删除底层文件可能影响外部业务系统、历史任务和已复制的预览地址。仅删除台账不会删除 Agent 上的文件。",
      `删除 ${asset.file_name}`,
      {
        type: "warning",
        confirmButtonText: "同步删除底层文件",
        cancelButtonText: "仅删除台账",
        distinguishCancelAndClose: true,
      },
    );
    deleteFile = true;
  } catch (action) {
    if (action !== "cancel") {
      return;
    }
  }

  try {
    await mediaUploadApi.remove(asset.id, deleteFile);
    ElMessage.success(deleteFile ? "已删除台账和底层文件" : "已删除台账");
    await assetsQuery.refetch();
  } catch (error) {
    ElMessage.error(errorMessage(error));
  }
}
</script>

<template>
  <section class="page-grid">
    <PageHeader title="媒资上传" description="上传单个点播文件，生成后续任务可引用的 file 输入路径和预览 HTTP 地址。">
      <div class="table-actions">
        <el-button :icon="Refresh" :loading="nodesQuery.isFetching.value" @click="nodesQuery.refetch()">刷新节点</el-button>
        <el-button :icon="Refresh" :loading="assetsQuery.isFetching.value" @click="assetsQuery.refetch()">刷新产物</el-button>
      </div>
    </PageHeader>

    <div class="metric-grid">
      <div class="surface-card metric-card">
        <span class="subtle">可上传节点</span>
        <strong>{{ uploadReadyNodes.length }}</strong>
      </div>
      <div class="surface-card metric-card">
        <span class="subtle">上传产物</span>
        <strong>{{ assetsQuery.data.value?.total ?? 0 }}</strong>
      </div>
      <div class="surface-card metric-card">
        <span class="subtle">上传耗时</span>
        <strong>{{ elapsedLabel(elapsedMs) }}</strong>
      </div>
    </div>

    <div class="surface-card upload-layout">
      <section class="upload-section">
        <h2 class="page-section-title">上传文件</h2>
        <button
          type="button"
          class="upload-zone"
          :class="{ dragging }"
          :disabled="uploading"
          @click="chooseFile"
          @dragover.prevent="dragging = true"
          @dragleave.prevent="dragging = false"
          @drop.prevent="onDrop"
        >
          <el-icon class="upload-zone-icon"><UploadFilled /></el-icon>
          <strong>{{ selectedFile?.name ?? "选择或拖入视频文件" }}</strong>
          <span>{{ selectedFile ? `${formatBytes(selectedFile.size)} · ${selectedExtension.toUpperCase()}` : "MP4 / MOV / MKV / WebM / TS / FLV" }}</span>
        </button>
        <input ref="fileInput" class="file-input" type="file" :accept="accept" @change="onFileInputChange" />

        <div class="target-panel">
          <el-radio-group v-model="targetMode">
            <el-radio-button label="auto">自动选择</el-radio-button>
            <el-radio-button label="node">指定节点</el-radio-button>
            <el-radio-button label="labels">指定标签</el-radio-button>
          </el-radio-group>
          <el-select v-if="targetMode === 'node'" v-model="selectedNodeId" filterable placeholder="选择上传节点" style="width: 100%">
            <el-option
              v-for="node in uploadReadyNodes"
              :key="node.id"
              :label="`${node.node_name} · ${formatBytes(node.upload_disk_available_bytes)}`"
              :value="node.id"
            />
          </el-select>
          <el-input v-if="targetMode === 'labels'" v-model="requiredLabelsText" placeholder="objective,room-a" />
        </div>

        <el-alert
          v-if="uploadError"
          class="upload-alert"
          type="error"
          :title="uploadError"
          :closable="false"
          show-icon
        />

        <div v-if="uploadPhase !== 'idle'" class="upload-progress-panel">
          <div class="upload-progress-header">
            <span>{{ uploadProgressLabel }}</span>
          </div>
          <el-progress :percentage="uploadProgress" :status="uploadProgressStatus" />
        </div>

        <div class="table-actions upload-actions">
          <el-button type="primary" :icon="Upload" :loading="uploading" :disabled="!selectedFile" @click="submitUpload">
            开始上传
          </el-button>
          <el-button :icon="Delete" :disabled="uploading || !selectedFile" @click="clearSelection">清空</el-button>
        </div>
      </section>

      <section class="upload-section">
        <h2 class="page-section-title">节点状态</h2>
        <div class="table-scroll">
          <el-table :data="nodesQuery.data.value ?? []" v-loading="nodesQuery.isLoading.value" size="small">
            <el-table-column label="节点" min-width="160">
              <template #default="{ row }">
                <div>{{ row.node_name }}</div>
                <div class="subtle">{{ shortId(row.id) }}</div>
              </template>
            </el-table-column>
            <el-table-column label="标签" min-width="180">
              <template #default="{ row }">
                <el-tag v-for="label in nodeLabels(row)" :key="label" class="label-tag" size="small">{{ label }}</el-tag>
                <span v-if="!nodeLabels(row).length" class="subtle">—</span>
              </template>
            </el-table-column>
            <el-table-column label="上传盘剩余" min-width="120">
              <template #default="{ row }">{{ formatBytes(row.upload_disk_available_bytes) }}</template>
            </el-table-column>
            <el-table-column label="上传盘使用率" min-width="120">
              <template #default="{ row }">{{ formatPercent(row.upload_disk_used_percent) }}</template>
            </el-table-column>
            <el-table-column label="槽位" min-width="90">
              <template #default="{ row }">{{ formatPercent((row.slot_usage ?? 0) * 100) }}</template>
            </el-table-column>
            <el-table-column label="状态" min-width="100">
              <template #default="{ row }">
                <el-tag v-if="nodeUploadReady(row)" type="success">可用</el-tag>
                <el-tag v-else type="warning">不可用</el-tag>
              </template>
            </el-table-column>
          </el-table>
        </div>
      </section>
    </div>

    <div v-if="uploadResult" class="surface-card">
      <h2 class="page-section-title">上传结果</h2>
      <el-descriptions :column="1" border>
        <el-descriptions-item label="文件名">{{ uploadResult.fileName }}</el-descriptions-item>
        <el-descriptions-item label="落盘节点">{{ uploadedNode?.node_name ?? shortId(uploadedNodeId) }}</el-descriptions-item>
        <el-descriptions-item label="Source URL">{{ uploadResult.sourceUrl }}</el-descriptions-item>
        <el-descriptions-item label="HTTP URL">{{ uploadResult.httpUrl }}</el-descriptions-item>
        <el-descriptions-item label="时长">{{ uploadResult.durationSec }}s</el-descriptions-item>
        <el-descriptions-item label="大小">{{ formatBytes(uploadResult.fileSize) }}</el-descriptions-item>
        <el-descriptions-item label="SHA-256">{{ uploadResult.sha256 }}</el-descriptions-item>
        <el-descriptions-item label="Content-Type">{{ uploadResult.contentType }}</el-descriptions-item>
        <el-descriptions-item label="创建时间">{{ uploadCreatedAt(uploadResult.createdAt) }}</el-descriptions-item>
      </el-descriptions>

      <div class="table-actions upload-actions">
        <el-button :icon="CopyDocument" @click="copyText(uploadResult.sourceUrl).then(() => ElMessage.success('已复制 Source URL'))">
          复制 Source URL
        </el-button>
        <el-button :icon="CopyDocument" @click="copyText(uploadResult.httpUrl).then(() => ElMessage.success('已复制 HTTP URL'))">
          复制 HTTP URL
        </el-button>
        <MediaLink :url="uploadResult.httpUrl">
          打开文件
        </MediaLink>
      </div>
    </div>

    <div class="surface-card">
      <div class="section-heading-row">
        <h2 class="page-section-title">上传产物</h2>
        <div class="table-actions">
          <el-select v-model="assetStatus" style="width: 120px" @change="assetsQuery.refetch()">
            <el-option label="有效" value="active" />
            <el-option label="已删除" value="deleted" />
            <el-option label="全部" value="all" />
          </el-select>
          <el-select v-model="assetNodeId" clearable filterable placeholder="全部节点" style="width: 180px" @change="assetsQuery.refetch()">
            <el-option v-for="node in nodesQuery.data.value ?? []" :key="node.id" :label="node.node_name" :value="node.id" />
          </el-select>
          <el-input v-model="assetKeyword" clearable placeholder="文件名 / Source URL / SHA-256" style="width: 260px" @keyup.enter="assetsQuery.refetch()" />
          <el-button :icon="Refresh" :loading="assetsQuery.isFetching.value" @click="assetsQuery.refetch()">查询</el-button>
        </div>
      </div>

      <div class="table-scroll">
        <el-table :data="uploadAssets" v-loading="assetsQuery.isLoading.value">
          <el-table-column label="文件" min-width="220">
            <template #default="{ row }">
              <div>{{ row.file_name }}</div>
              <div class="subtle">{{ shortId(row.id) }}</div>
            </template>
          </el-table-column>
          <el-table-column label="节点" min-width="150">
            <template #default="{ row }">{{ row.node_name }}</template>
          </el-table-column>
          <el-table-column prop="source_url" label="Source URL" min-width="300" />
          <el-table-column label="大小" min-width="110">
            <template #default="{ row }">{{ formatBytes(row.file_size) }}</template>
          </el-table-column>
          <el-table-column label="时长" min-width="90">
            <template #default="{ row }">{{ row.duration_sec }}s</template>
          </el-table-column>
          <el-table-column label="状态" min-width="110">
            <template #default="{ row }">
              <el-tag v-if="row.status === 'active'" type="success">有效</el-tag>
              <el-tag v-else type="info">{{ row.file_deleted ? "文件已删" : "台账已删" }}</el-tag>
            </template>
          </el-table-column>
          <el-table-column label="创建时间" min-width="170">
            <template #default="{ row }">{{ formatTime(row.created_at) }}</template>
          </el-table-column>
          <el-table-column label="操作" fixed="right" min-width="260">
            <template #default="{ row }">
              <el-button link @click="copyText(row.source_url).then(() => ElMessage.success('已复制 Source URL'))">复制路径</el-button>
              <el-button link @click="copyText(row.http_url).then(() => ElMessage.success('已复制 HTTP URL'))">复制 HTTP</el-button>
              <MediaLink :url="row.http_url" label="打开" />
              <el-button v-if="row.status === 'active'" link type="danger" @click="deleteAsset(row)">删除</el-button>
            </template>
          </el-table-column>
        </el-table>
      </div>
    </div>
  </section>
</template>

<style scoped>
.upload-layout {
  display: grid;
  grid-template-columns: minmax(320px, 0.9fr) minmax(0, 1.1fr);
  gap: 22px;
}

.upload-section {
  min-width: 0;
}

.upload-zone {
  width: 100%;
  min-height: 210px;
  border: 1px dashed var(--console-border-strong);
  border-radius: var(--console-radius-md);
  background: rgba(15, 23, 42, 0.025);
  color: var(--console-text);
  display: grid;
  place-items: center;
  align-content: center;
  gap: 10px;
  padding: 24px;
  cursor: pointer;
  transition:
    border-color 0.18s ease,
    background 0.18s ease,
    color 0.18s ease;
}

.upload-zone:hover,
.upload-zone.dragging {
  border-color: var(--console-primary);
  background: var(--console-primary-soft);
}

.upload-zone:disabled {
  cursor: not-allowed;
  opacity: 0.72;
}

.upload-zone strong,
.upload-zone span {
  max-width: 100%;
  overflow-wrap: anywhere;
  text-align: center;
}

.upload-zone-icon {
  font-size: 40px;
  color: var(--console-primary);
}

.file-input {
  position: absolute;
  width: 1px;
  height: 1px;
  opacity: 0;
  pointer-events: none;
}

.target-panel {
  display: grid;
  gap: 12px;
  margin-top: 16px;
}

.upload-alert,
.upload-actions {
  margin-top: 16px;
}

.upload-progress-panel {
  display: grid;
  gap: 8px;
  margin-top: 16px;
}

.upload-progress-header {
  display: flex;
  justify-content: space-between;
  gap: 12px;
  color: var(--console-text);
  font-size: 13px;
}

.upload-actions {
  align-items: center;
}

.upload-actions .el-link {
  display: inline-flex;
  align-items: center;
  gap: 4px;
}

.label-tag {
  margin: 0 6px 6px 0;
}

.section-heading-row {
  display: flex;
  justify-content: space-between;
  gap: 16px;
  align-items: flex-start;
  flex-wrap: wrap;
}

@media (max-width: 1100px) {
  .upload-layout {
    grid-template-columns: 1fr;
  }
}
</style>
