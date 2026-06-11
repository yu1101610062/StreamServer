<script setup lang="ts">
import { computed, reactive, ref, watch } from "vue";
import { useRouter } from "vue-router";
import { useMutation, useQuery } from "@tanstack/vue-query";
import { ElMessage } from "element-plus";

import { activeSession } from "@/shared/api/client";
import { mediaUploadApi, nodeApi, taskApi } from "@/shared/api/resources";
import type { MediaUploadAssetSummary, TaskPreview } from "@/shared/api/types";
import PageHeader from "@/shared/components/PageHeader.vue";
import {
  buildDraftPayload,
  collectDraftWhitespaceIssues,
  collectTaskPayloadWhitespaceIssues,
  createDefaultDraft,
  defaultSourceModeForInputKind,
  type DraftWhitespaceIssue,
  deriveStreamIngestRecordMode,
  guidedScenarios,
  humanSummary,
  inputKindSupportsLoop,
  inputKindSupportsExplicitSourceMode,
  normalizeDraftForTaskType,
  optionSets,
} from "@/shared/task-create";
import { formatBytes, formatJson, formatTime, taskValidationMessage } from "@/shared/utils/format";

const router = useRouter();
const draft = reactive(createDefaultDraft());
const createMode = ref<"guided" | "expert">("guided");
const selectedScenario = ref(guidedScenarios[0].id);
const previewData = ref<TaskPreview | null>(null);
const selectedUploadAssetId = ref("");
const currentCreator = computed(() => activeSession.value?.subject?.trim() ?? "");
type TaskCreatePayload = ReturnType<typeof buildDraftPayload>;

const nodeFormatsQuery = useQuery({
  queryKey: ["task-create-publish-formats"],
  queryFn: () => nodeApi.list(),
  retry: false,
  staleTime: 60_000,
});

const uploadAssetsQuery = useQuery({
  queryKey: ["task-create", "upload-assets"],
  queryFn: () => mediaUploadApi.list({ status: "active", page_size: 100 }),
  retry: false,
  staleTime: 30_000,
});

const previewMutation = useMutation({
  mutationFn: (payload: TaskCreatePayload) => taskApi.preview(payload),
  onSuccess: (result) => {
    previewData.value = result;
    ElMessage.success("规格检查通过，已生成解析结果");
  },
  onError: (error) => ElMessage.error(taskValidationMessage(error)),
});

const createMutation = useMutation({
  mutationFn: (payload: TaskCreatePayload) => taskApi.create(payload),
  onSuccess: async (task) => {
    ElMessage.success("任务已创建");
    await router.push(`/tasks/${task.id}`);
  },
  onError: (error) => ElMessage.error(taskValidationMessage(error)),
});

const showExplicitSourceMode = computed(() => inputKindSupportsExplicitSourceMode(draft.input.kind));
const showInputUrl = computed(() =>
  ["rtsp", "rtmp", "hls", "ftp", "http_mp4", "http_flv", "http_ts"].includes(draft.input.kind),
);
const showInputMulticast = computed(() =>
  ["udp_mpegts_multicast", "rtp_multicast"].includes(draft.input.kind),
);
const showGbRtp = computed(() => draft.input.kind === "gb_rtp");
const showManagedFileInputHint = computed(() => draft.input.kind === "file");
const showFtpInputHint = computed(() => draft.input.kind === "ftp");
const inputUrlPlaceholder = computed(() => {
  if (draft.input.kind === "file") {
    return "demo.mp4 或 vod/demo.ts";
  }
  if (draft.input.kind === "ftp") {
    return "ftp://user:pass@example.com/archive/demo.mp4";
  }
  return "rtsp:// / rtmp:// / http://...";
});
const showInputLoop = computed(
  () =>
    draft.task_type === "stream_ingest" &&
    inputKindSupportsLoop(draft.input.kind, draft.input.source_mode),
);
const showPublishSection = computed(() => draft.task_type !== "stream_ingest");
const showManagedFileOutputHint = computed(
  () => showPublishSection.value && draft.publish.kind === "file",
);
const showPublishMulticast = computed(() =>
  ["udp_mpegts_multicast", "rtp_multicast"].includes(draft.publish.kind),
);
const showPublishRtmpUrl = computed(
  () => showPublishSection.value && draft.publish.kind === "rtmp_push",
);
const showPublishFormatSelect = computed(
  () => showPublishSection.value && draft.publish.kind !== "rtmp_push",
);
const showStreamSection = computed(() => draft.task_type === "stream_ingest");
const showRecordSection = computed(() => draft.task_type === "stream_ingest");
const derivedRecordMode = computed(() => deriveStreamIngestRecordMode(draft));
const previewPayload = computed(() => buildDraftPayload(draft));
const summaryText = computed(() => humanSummary(draft));
const whitespaceIssues = computed(() => collectDraftWhitespaceIssues(draft));
const activeUploadAssets = computed(() =>
  (uploadAssetsQuery.data.value?.items ?? []).filter((asset) => asset.status === "active" && !asset.file_deleted),
);
const selectedUploadAsset = computed(
  () =>
    activeUploadAssets.value.find((asset) => asset.id === selectedUploadAssetId.value) ??
    activeUploadAssets.value.find((asset) => asset.source_url === draft.input.url) ??
    null,
);
const managedFileInputHint = computed(
  () =>
    "文件输入使用媒资上传产生的 Source URL。选择产物后会自动填入任务规格，任务调度会按路径中的节点信息保持亲和。",
);
const ftpInputHint = computed(
  () =>
    "FTP 输入只支持 ftp:// 远端文件/VOD 地址，不支持 ftps://。如需认证，请直接在 URL 中携带用户名和密码，例如 ftp://user:pass@example.com/archive/demo.mp4。",
);
const inputLoopHint = computed(
  () =>
    "开启后，系统会在离线输入读到 EOF 后从头继续读取，适合让内部流长期保持有内容。如果同时设置录制时长，到时任务仍会整体成功结束。",
);
const recordModeHintTitle = computed(() => {
  if (derivedRecordMode.value === "realtime") {
    return "当前会按实时模式录制";
  }
  if (derivedRecordMode.value === "fast") {
    return "当前会按快录模式录制";
  }
  return "";
});
const recordModeHint = computed(() => {
  if (derivedRecordMode.value === "realtime") {
    return "因为你仍启用了至少一种播放协议，系统会继续生成内部流并按实时节奏录制，旧行为保持不变。";
  }
  if (derivedRecordMode.value === "fast") {
    return "因为你关闭了全部播放协议，系统会跳过内部流播放链路并尽快完成录制。此时不会提供实时流播放地址；如果还开启了循环读取，建议同时填写录制时长。";
  }
  return "";
});
const publishFormatGroups = computed(() => {
  const commonOptions = optionSets.publishFormats;
  const knownValues = new Set(commonOptions.map((item) => item.value));
  const dynamicOptions = new Map<string, { value: string; label: string; note?: string }>();
  (nodeFormatsQuery.data.value ?? []).forEach((node) => {
    node.ffmpeg_formats.forEach((format) => {
      const normalized = format.trim();
      if (!normalized || knownValues.has(normalized) || !/^[A-Za-z0-9][A-Za-z0-9_.+-]*$/.test(normalized)) {
        return;
      }
      if (!dynamicOptions.has(normalized)) {
        dynamicOptions.set(normalized, {
          value: normalized,
          label: normalized,
          note: "来自节点上报的 FFmpeg 格式能力",
        });
      }
    });
  });
  const groups = [
    {
      label: "常用封装格式",
      options: commonOptions,
    },
  ];
  const extraOptions = Array.from(dynamicOptions.values()).sort((left, right) => left.label.localeCompare(right.label));
  if (extraOptions.length) {
    groups.push({
      label: "节点扩展格式",
      options: extraOptions,
    });
  }
  return groups;
});

watch(
  () => draft.task_type,
  (taskType) => {
    normalizeDraftForTaskType(draft, taskType);
  },
);

watch(
  () => draft.input.kind,
  (kind) => {
    const fixedMode = defaultSourceModeForInputKind(kind);
    if (fixedMode) {
      draft.input.source_mode = fixedMode;
    } else if (!inputKindSupportsExplicitSourceMode(kind)) {
      draft.input.source_mode = "";
    }
    if (draft.task_type === "file_transcode") {
      draft.publish.kind = "file";
    }
    if (kind === "file") {
      uploadAssetsQuery.refetch();
    } else {
      selectedUploadAssetId.value = "";
    }
  },
);

watch(
  showInputLoop,
  (enabled) => {
    if (!enabled) {
      draft.input.loop_enabled = false;
    }
  },
  { immediate: true },
);

watch(
  () => draft.publish.kind,
  (kind) => {
    if (kind === "file") {
      draft.publish.url = "";
    }
    if (kind === "rtmp_push") {
      draft.publish.group = "";
      draft.publish.port = "";
      draft.publish.interface_name = "";
      draft.publish.interface_ip = "";
      draft.publish.ttl = "";
      draft.publish.format = "";
    } else if (["udp_mpegts_multicast", "rtp_multicast"].includes(kind)) {
      draft.publish.url = "";
    }
  },
);

watch(
  [currentCreator, () => draft.common.created_by],
  ([subject, createdBy]) => {
    if (!createdBy.trim() && subject) {
      draft.common.created_by = subject;
    }
  },
  { immediate: true },
);

watch(
  () => JSON.stringify(draft),
  () => {
    previewData.value = null;
  },
);

function applyScenario(id: string) {
  const scenario = guidedScenarios.find((item) => item.id === id);
  if (!scenario) return;
  selectedScenario.value = id;
  Object.assign(draft, createDefaultDraft());
  scenario.apply(draft);
  normalizeDraftForTaskType(draft, draft.task_type);
  previewData.value = null;
}

function updateTaskType(value: string) {
  draft.task_type = value;
  previewData.value = null;
}

function updateInputKind(value: string) {
  draft.input.kind = value;
  previewData.value = null;
}

function selectUploadAsset(assetId: string) {
  selectedUploadAssetId.value = assetId;
  const asset = activeUploadAssets.value.find((item: MediaUploadAssetSummary) => item.id === assetId);
  draft.input.url = asset?.source_url ?? "";
  previewData.value = null;
}

function fieldWhitespaceError(field: DraftWhitespaceIssue["field"]) {
  return whitespaceIssues.value.find((issue) => issue.field === field)?.message ?? "";
}

function buildValidatedPayload() {
  const draftIssue = whitespaceIssues.value[0];
  if (draftIssue) {
    ElMessage.error(draftIssue.message);
    return null;
  }
  const payload = buildDraftPayload(draft);
  const payloadIssue = collectTaskPayloadWhitespaceIssues(payload)[0];
  if (payloadIssue) {
    ElMessage.error(payloadIssue.message);
    return null;
  }
  return payload;
}

function createTask() {
  const payload = buildValidatedPayload();
  if (payload) {
    createMutation.mutate(payload);
  }
}

function previewTask() {
  const payload = buildValidatedPayload();
  if (payload) {
    previewMutation.mutate(payload);
  }
}
</script>

<template>
  <section class="page-grid">
    <PageHeader title="新建任务" description="默认用引导式创建，帮助非技术同学先说明目标、再填写必要信息；熟悉规格的人可以切换到专家模式。" >
      <el-button @click="router.push('/tasks')">返回任务中心</el-button>
    </PageHeader>

    <div class="surface-card summary-banner">
      <div class="table-actions" style="justify-content: space-between">
        <div>
          <div class="page-kicker" style="color: var(--console-primary)">当前摘要</div>
          <strong>{{ summaryText }}</strong>
          <p class="subtle" style="margin: 8px 0 0">填写过程中会自动生成这段自然语言摘要，方便确认“这项任务到底要做什么”。</p>
        </div>
        <el-radio-group v-model="createMode">
          <el-radio-button label="guided">引导式创建</el-radio-button>
          <el-radio-button label="expert">专家模式</el-radio-button>
        </el-radio-group>
      </div>
    </div>

    <div class="surface-card">
      <h3 class="page-section-title">推荐场景</h3>
      <p class="subtle">这一步不会创建任何后端模板，只是帮你快速落到一个更接近目标的起点。</p>
      <div class="guide-card-grid">
        <div
          v-for="scenario in guidedScenarios"
          :key="scenario.id"
          class="surface-panel guide-card"
          :class="{ active: selectedScenario === scenario.id }"
          style="padding: 18px"
          @click="applyScenario(scenario.id)"
        >
          <strong>{{ scenario.title }}</strong>
          <p class="subtle" style="margin: 10px 0 0">{{ scenario.description }}</p>
        </div>
      </div>
    </div>

    <div class="surface-card">
      <el-steps finish-status="success" align-center>
        <el-step title="任务目标" description="先决定要把源接成内部流、桥接输出，还是做离线转码。" />
        <el-step title="输入源" description="说明源来自哪里，以及它是实时流还是离线点播。" />
        <el-step title="输出方式" :description="showStreamSection ? '决定内部流名称、播放协议和录制方式。' : '决定输出到文件、组播还是外部 RTMP。'" />
        <el-step title="恢复与调度" description="决定失败是否自动恢复，以及创建后什么时候真正启动。" />
        <el-step title="检查并创建" description="先看自然语言摘要和解析规格，再提交。" />
      </el-steps>
    </div>

    <div class="surface-card section-stack">
      <div>
        <h3 class="page-section-title">1. 任务目标</h3>
        <el-alert
          type="info"
          :closable="false"
          title="先选任务类型"
          description="流接入适合把源纳入平台统一管理；流桥接适合直接写文件或发组播；文件转码只做离线产物。"
        />
      </div>

      <el-form label-position="top">
        <el-row :gutter="16">
          <el-col :md="8" :span="24">
            <el-form-item label="任务名称" :error="fieldWhitespaceError('name')">
              <el-input v-model="draft.name" placeholder="给这项任务起一个便于业务识别的名字" />
            </el-form-item>
          </el-col>
          <el-col :md="8" :span="24">
            <el-form-item label="任务类型">
              <el-select v-model="draft.task_type" style="width: 100%" @update:model-value="updateTaskType">
                <el-option v-for="item in optionSets.taskTypes" :key="item.value" :label="item.label" :value="item.value">
                  <div>
                    <strong>{{ item.label }}</strong>
                    <div class="subtle">{{ item.note }}</div>
                  </div>
                </el-option>
              </el-select>
            </el-form-item>
          </el-col>
          <el-col :md="8" :span="24">
            <el-form-item label="优先级">
              <el-input v-model="draft.priority" placeholder="默认 50，数值越大越优先" />
            </el-form-item>
          </el-col>
        </el-row>
      </el-form>
    </div>

    <div class="surface-card section-stack">
      <div>
        <h3 class="page-section-title">2. 输入源</h3>
        <el-alert
          type="info"
          :closable="false"
          title="用业务语言理解 source_mode"
          description="如果源在你点击开始时就持续产生数据，选实时源；如果它是文件或点播地址，通常选离线源。HLS / HTTP-TS 这两类协议必须你手动说清楚。"
        />
      </div>

      <el-form label-position="top">
        <el-row :gutter="16">
          <el-col :md="8" :span="24">
            <el-form-item label="输入类型">
              <el-select v-model="draft.input.kind" style="width: 100%" @update:model-value="updateInputKind">
                <el-option v-for="item in optionSets.inputKinds" :key="item.value" :label="item.label" :value="item.value" />
              </el-select>
            </el-form-item>
          </el-col>
          <el-col v-if="showExplicitSourceMode" :md="8" :span="24">
            <el-form-item label="源模式">
              <el-select v-model="draft.input.source_mode" style="width: 100%">
                <el-option v-for="item in optionSets.sourceModes" :key="item.value" :label="item.label" :value="item.value" />
              </el-select>
            </el-form-item>
          </el-col>
          <el-col v-else :md="8" :span="24">
            <el-form-item label="源模式">
              <el-input :model-value="draft.input.source_mode === 'live' ? '实时源' : draft.input.source_mode === 'vod' ? '离线源' : '自动'" disabled />
            </el-form-item>
          </el-col>
          <el-col :md="8" :span="24">
            <el-form-item label="探测超时（毫秒）">
              <el-input v-model="draft.input.probe_timeout_ms" placeholder="默认 7000" />
            </el-form-item>
          </el-col>
        </el-row>

        <el-row v-if="showInputUrl" :gutter="16">
          <el-col :span="24">
            <el-form-item :label="draft.input.kind === 'file' ? '输入文件相对路径' : '输入 URL'">
              <el-input
                v-model="draft.input.url"
                :placeholder="inputUrlPlaceholder"
              />
            </el-form-item>
          </el-col>
        </el-row>

        <el-row v-if="showManagedFileInputHint" :gutter="16">
          <el-col :span="24">
            <el-form-item label="上传媒资">
              <el-select
                v-model="selectedUploadAssetId"
                filterable
                placeholder="选择已上传媒资"
                style="width: 100%"
                :loading="uploadAssetsQuery.isFetching.value"
                @update:model-value="selectUploadAsset"
              >
                <el-option
                  v-for="asset in activeUploadAssets"
                  :key="asset.id"
                  :label="`${asset.file_name} · ${asset.node_name} · ${formatBytes(asset.file_size)}`"
                  :value="asset.id"
                >
                  <div class="asset-option">
                    <span>{{ asset.file_name }}</span>
                    <span class="subtle">{{ asset.node_name }} · {{ formatBytes(asset.file_size) }} · {{ formatTime(asset.created_at) }}</span>
                  </div>
                </el-option>
              </el-select>
            </el-form-item>
          </el-col>
        </el-row>

        <el-descriptions v-if="selectedUploadAsset" :column="1" border size="small">
          <el-descriptions-item label="Source URL">{{ selectedUploadAsset.source_url }}</el-descriptions-item>
          <el-descriptions-item label="HTTP URL">{{ selectedUploadAsset.http_url }}</el-descriptions-item>
          <el-descriptions-item label="落盘节点">{{ selectedUploadAsset.node_name }}</el-descriptions-item>
        </el-descriptions>

        <el-alert
          v-if="showManagedFileInputHint"
          type="info"
          :closable="false"
          title="本地文件输入只填相对路径"
          :description="managedFileInputHint"
        />

        <el-alert
          v-if="showFtpInputHint"
          type="info"
          :closable="false"
          title="FTP 输入仅支持 ftp://"
          :description="ftpInputHint"
        />

        <el-row v-if="showInputLoop" :gutter="16">
          <el-col :md="8" :span="24">
            <el-form-item label="循环读取离线输入">
              <el-switch v-model="draft.input.loop_enabled" />
            </el-form-item>
          </el-col>
          <el-col :md="16" :span="24">
            <el-alert
              type="info"
              :closable="false"
              title="适合让内部流长期保持可播"
              :description="inputLoopHint"
            />
          </el-col>
        </el-row>

        <el-row v-if="showInputMulticast" :gutter="16">
          <el-col :md="8" :span="24">
            <el-form-item label="组播地址">
              <el-input v-model="draft.input.group" placeholder="239.0.0.10" />
            </el-form-item>
          </el-col>
          <el-col :md="8" :span="24">
            <el-form-item label="端口">
              <el-input v-model="draft.input.port" placeholder="1234" />
            </el-form-item>
          </el-col>
          <el-col :md="8" :span="24">
            <el-form-item label="绑定网卡 IP">
              <el-input v-model="draft.input.interface_ip" placeholder="10.0.0.12" />
            </el-form-item>
          </el-col>
        </el-row>

        <el-row v-if="showGbRtp" :gutter="16">
          <el-col :md="8" :span="24">
            <el-form-item label="监听端口">
              <el-input v-model="draft.input.port" placeholder="30000" />
            </el-form-item>
          </el-col>
          <el-col :md="8" :span="24">
            <el-form-item label="TCP 模式">
              <el-input v-model="draft.input.tcp_mode" placeholder="0=UDP, 1=TCP passive, 2=TCP active" />
            </el-form-item>
          </el-col>
          <el-col :md="8" :span="24">
            <el-form-item label="SSRC">
              <el-input v-model="draft.input.ssrc" placeholder="可选" />
            </el-form-item>
          </el-col>
        </el-row>
      </el-form>
    </div>

    <div class="surface-card section-stack">
      <div>
        <h3 class="page-section-title">3. 处理方式</h3>
        <el-alert
          v-if="createMode === 'guided'"
          type="success"
          :closable="false"
          title="推荐先用“拷贝优先，必要时转码”"
          description="这会优先复用原始码流，只在协议或编码不兼容时才转码，通常是最稳妥的默认值。"
        />
      </div>
      <el-form label-position="top">
        <el-row :gutter="16">
          <el-col :md="8" :span="24">
            <el-form-item label="处理策略">
              <el-select v-model="draft.process.mode" style="width: 100%">
                <el-option v-for="item in optionSets.processModes" :key="item.value" :label="item.label" :value="item.value" />
              </el-select>
            </el-form-item>
          </el-col>
          <el-col :md="8" :span="24">
            <el-form-item label="目标码率（可选）">
              <el-input v-model="draft.process.bitrate" placeholder="kbps，例如 2500" />
            </el-form-item>
          </el-col>
          <el-col :md="8" :span="24">
            <el-form-item label="帧率 / GOP（可选）">
              <div class="split-inline-fields">
                <el-input v-model="draft.process.fps" placeholder="fps" />
                <el-input v-model="draft.process.gop" placeholder="gop" />
              </div>
            </el-form-item>
          </el-col>
        </el-row>
      </el-form>
    </div>

    <div v-if="showStreamSection" class="surface-card section-stack">
      <div>
        <h3 class="page-section-title">4. 内部流与播放暴露</h3>
        <el-alert
          type="info"
          :closable="false"
          title="这里不是外部推流目标"
          description="流接入任务会先形成平台内部流。app / stream 决定内部流名称；勾选的协议决定别人之后可以用哪些方式来播。"
        />
      </div>
      <el-form label-position="top">
        <el-row :gutter="16">
          <el-col :md="8" :span="24">
            <el-form-item label="内部应用名" :error="fieldWhitespaceError('stream.app')">
              <el-input v-model="draft.stream.app" placeholder="live" />
            </el-form-item>
          </el-col>
          <el-col :md="8" :span="24">
            <el-form-item label="内部流名" :error="fieldWhitespaceError('stream.name')">
              <el-input v-model="draft.stream.name" placeholder="建议填业务友好的流名，例如 camera01" />
            </el-form-item>
          </el-col>
          <el-col :md="8" :span="24">
            <el-form-item label="Vhost" :error="fieldWhitespaceError('stream.vhost')">
              <el-input v-model="draft.stream.vhost" placeholder="__defaultVhost__" />
            </el-form-item>
          </el-col>
        </el-row>

        <el-form-item label="对外播放协议">
          <div class="checkbox-grid">
            <el-checkbox v-model="draft.expose.enable_rtsp">RTSP</el-checkbox>
            <el-checkbox v-model="draft.expose.enable_rtmp">RTMP</el-checkbox>
            <el-checkbox v-model="draft.expose.enable_http_ts">HTTP-TS</el-checkbox>
            <el-checkbox v-model="draft.expose.enable_http_fmp4">HTTP-FMP4</el-checkbox>
            <el-checkbox v-model="draft.expose.enable_hls">HLS</el-checkbox>
            <el-checkbox v-model="draft.expose.stop_on_no_reader">无人观看时自动停流</el-checkbox>
          </div>
        </el-form-item>

        <el-alert
          v-if="derivedRecordMode === 'fast'"
          type="warning"
          :closable="false"
          title="当前配置不会启用实时流播放"
          description="你已经关闭了全部播放协议。若同时开启录制，系统会直接进入快录分支，不再保留可播放的内部流。app / stream / vhost 仅保留为任务规格字段，不会形成实时播放地址。"
        />
      </el-form>
    </div>

    <div v-if="showPublishSection" class="surface-card section-stack">
      <div>
        <h3 class="page-section-title">4. 直接输出目标</h3>
        <el-alert
          type="info"
          :closable="false"
          title="桥接和离线转码都需要明确输出目标"
          description="文件输出用于导出文件；组播用于把流直接发到网络目标；RTMP / RTMPS 推流用于把流直接送到外部流媒体平台。"
        />
      </div>
      <el-form label-position="top">
        <el-row :gutter="16">
          <el-col :md="8" :span="24">
            <el-form-item label="输出类型">
              <el-select v-model="draft.publish.kind" style="width: 100%">
                <el-option v-for="item in optionSets.publishKinds" :key="item.value" :label="item.label" :value="item.value" />
              </el-select>
            </el-form-item>
          </el-col>
          <el-col v-if="showPublishFormatSelect" :md="8" :span="24">
            <el-form-item label="输出封装格式（可选）">
              <el-select v-model="draft.publish.format" style="width: 100%" clearable filterable placeholder="选择输出封装格式">
                <el-option-group v-for="group in publishFormatGroups" :key="group.label" :label="group.label">
                  <el-option
                    v-for="item in group.options"
                    :key="item.value || `__auto__-${group.label}`"
                    :label="item.label"
                    :value="item.value"
                  >
                    <div>
                      <strong>{{ item.label }}</strong>
                      <div v-if="item.note" class="subtle">{{ item.note }}</div>
                    </div>
                  </el-option>
                </el-option-group>
              </el-select>
              <div class="subtle" style="margin-top: 8px">常用封装格式固定置顶。文件输出不填时默认 MP4；更多扩展格式来自当前节点上报的 FFmpeg 能力。</div>
            </el-form-item>
          </el-col>
          <el-col v-if="showPublishRtmpUrl" :md="16" :span="24">
            <el-form-item label="推流目标 URL">
              <el-input v-model="draft.publish.url" placeholder="rtmp://push.example.com/live/stream01 或 rtmps://..." />
              <div class="subtle" style="margin-top: 8px">系统固定使用 FLV 封装推送到外部 RTMP / RTMPS 目标。离线源会自动按实时节奏推送，避免把下游瞬间灌满。</div>
            </el-form-item>
          </el-col>
        </el-row>

        <el-row v-if="showManagedFileOutputHint" :gutter="16">
          <el-col :span="24">
            <el-alert
              type="info"
              :closable="false"
              title="文件路径由平台托管"
              description="创建时不需要填写目录或文件名。系统会按执行节点本地时间自动生成路径：stream_bridge 写入 /data/zlm/www/artifacts/bridge/YYYY/MM/DD/HHMMSS[-NN].扩展名，file_transcode 写入 /data/zlm/www/artifacts/transcode/YYYY/MM/DD/HHMMSS[-NN].扩展名。"
            />
          </el-col>
        </el-row>

        <el-row v-if="showPublishMulticast" :gutter="16">
          <el-col :md="8" :span="24">
            <el-form-item label="组播地址">
              <el-input v-model="draft.publish.group" placeholder="239.0.0.10" />
            </el-form-item>
          </el-col>
          <el-col :md="8" :span="24">
            <el-form-item label="端口">
              <el-input v-model="draft.publish.port" placeholder="1234" />
            </el-form-item>
          </el-col>
          <el-col :md="8" :span="24">
            <el-form-item label="绑定地址 / 网卡">
              <el-input v-model="draft.publish.interface_ip" placeholder="10.0.0.12" />
            </el-form-item>
          </el-col>
        </el-row>
      </el-form>
    </div>

    <div v-if="showRecordSection" class="surface-card section-stack">
      <div>
        <h3 class="page-section-title">5. 录制</h3>
        <el-alert
          type="warning"
          :closable="false"
          title="只在你确实需要回看文件时开启录制"
          description="MP4 适合回看和下载，默认输出单个文件；HLS 适合分段输出；MP4 + HLS 适合同时兼顾两种需求。只有显式填写分段时长时，MP4 才会切片。录制目录由系统托管生成。VOD 输入是否快录会由播放协议自动判定。"
        />
      </div>
      <el-form label-position="top">
        <el-alert
          v-if="derivedRecordMode"
          :type="derivedRecordMode === 'fast' ? 'warning' : 'info'"
          :closable="false"
          :title="recordModeHintTitle"
          :description="recordModeHint"
          style="margin-bottom: 16px"
        />
        <el-row :gutter="16">
          <el-col :md="6" :span="24">
            <el-form-item label="启用录制">
              <el-switch v-model="draft.record.enabled" />
            </el-form-item>
          </el-col>
          <el-col v-if="draft.record.enabled" :md="6" :span="24">
            <el-form-item label="录制格式">
              <el-select v-model="draft.record.format" style="width: 100%">
                <el-option v-for="item in optionSets.recordFormats" :key="item.value" :label="item.label" :value="item.value" />
              </el-select>
            </el-form-item>
          </el-col>
          <el-col v-if="draft.record.enabled" :md="6" :span="24">
            <el-form-item label="录制时长（秒，可选）">
              <el-input v-model="draft.record.duration_sec" placeholder="例如 300" />
            </el-form-item>
          </el-col>
          <el-col v-if="draft.record.enabled" :md="6" :span="24">
            <el-form-item label="分段时长（秒，可选，仅 HLS 或需要切片 MP4 时填写）">
              <el-input v-model="draft.record.segment_sec" placeholder="例如 300；不填则按节点配置" />
            </el-form-item>
          </el-col>
          <el-col v-if="draft.record.enabled" :md="6" :span="24">
            <el-form-item label="按播放器视角录制">
              <el-switch v-model="draft.record.as_player" />
            </el-form-item>
          </el-col>
        </el-row>
      </el-form>
    </div>

    <div class="surface-card section-stack">
      <div>
        <h3 class="page-section-title">{{ showRecordSection ? "6" : "5" }}. 恢复与调度</h3>
        <el-alert
          type="info"
          :closable="false"
          title="把“恢复”和“什么时候开始”分开看"
          description="恢复策略决定异常后要不要自动拉起；启动模式决定这条任务在创建完成后是立刻跑、手动跑，还是等定时。"
        />
      </div>

      <el-form label-position="top">
        <el-row :gutter="16">
          <el-col :md="8" :span="24">
            <el-form-item label="恢复策略">
              <el-select v-model="draft.recovery.policy" style="width: 100%">
                <el-option v-for="item in optionSets.recoveryPolicies" :key="item.value" :label="item.label" :value="item.value" />
              </el-select>
            </el-form-item>
          </el-col>
          <el-col :md="8" :span="24">
            <el-form-item label="最大连续失败次数（可选）">
              <el-input v-model="draft.recovery.max_consecutive_failures" placeholder="为空表示按系统默认处理，仅限制启动失败" />
            </el-form-item>
          </el-col>
          <el-col :md="8" :span="24">
            <el-form-item label="恢复模式（高级，可选）">
              <el-input v-model="draft.recovery.resume_mode" placeholder="保留字段，通常留空" />
            </el-form-item>
          </el-col>
        </el-row>

        <el-row :gutter="16">
          <el-col :md="8" :span="24">
            <el-form-item label="启动模式">
              <el-select v-model="draft.schedule.start_mode" style="width: 100%">
                <el-option v-for="item in optionSets.startModes" :key="item.value" :label="item.label" :value="item.value" />
              </el-select>
            </el-form-item>
          </el-col>
          <el-col v-if="draft.schedule.start_mode === 'at'" :md="8" :span="24">
            <el-form-item label="指定启动时间">
              <el-date-picker
                v-model="draft.schedule.start_at"
                type="datetime"
                clearable
                format="YYYY-MM-DD HH:mm:ss"
                value-format="YYYY-MM-DDTHH:mm:ssZ"
                placeholder="选择启动时间"
              />
            </el-form-item>
          </el-col>
          <el-col v-if="draft.schedule.start_mode === 'cron'" :md="8" :span="24">
            <el-form-item label="Cron 表达式">
              <el-input v-model="draft.schedule.cron" placeholder="0 */5 * * * *" />
            </el-form-item>
          </el-col>
        </el-row>
      </el-form>
    </div>

    <div class="surface-card section-stack">
      <div>
        <h3 class="page-section-title">{{ showRecordSection ? "7" : "6" }}. 高级补充</h3>
        <p class="subtle">这些不是每次都要填。引导式建议先留空；专家模式可以用这里做精细控制。</p>
      </div>

      <el-form label-position="top">
        <el-row :gutter="16">
          <el-col :md="12" :span="24">
            <el-form-item label="创建人（自动）">
              <el-input :model-value="draft.common.created_by || currentCreator || '—'" readonly />
            </el-form-item>
          </el-col>
          <el-col :md="12" :span="24">
            <el-form-item label="任务回调地址（可选）">
              <el-input v-model="draft.common.callback_url" placeholder="https://biz.example.com/callback" />
            </el-form-item>
          </el-col>
        </el-row>
        <el-row :gutter="16">
          <el-col :md="12" :span="24">
            <el-form-item label="标签（逗号分隔，可选）">
              <el-input v-model="draft.common.labels_text" placeholder="project-a, night-shift" />
            </el-form-item>
          </el-col>
        </el-row>
        <el-row :gutter="16">
          <el-col :md="12" :span="24">
            <el-form-item label="节点必需标签（逗号分隔，可选）">
              <el-input v-model="draft.resource.required_labels_text" placeholder="gpu, beijing-idc" />
            </el-form-item>
          </el-col>
        </el-row>
        <el-alert
          type="info"
          :closable="false"
          title="节点必需标签会做硬过滤"
          description="任务只会派发到同时具备这些标签的节点；如果当前没有任何匹配标签的在线节点，任务会直接失败。"
        />

        <el-form-item v-if="createMode === 'expert'" label="高级 JSON 覆盖">
          <el-input
            v-model="draft.advanced_json"
            type="textarea"
            :rows="12"
            placeholder='{"process":{"profile":"high","preset":"fast"}}'
          />
        </el-form-item>
      </el-form>
    </div>

    <div class="surface-card section-stack">
      <div>
        <h3 class="page-section-title">{{ showRecordSection ? "8" : "7" }}. 检查并创建</h3>
        <p class="subtle">建议先点“检查规格”，确认系统如何解析你的输入；如果你已经很熟悉，也可以直接创建。</p>
      </div>

      <el-row :gutter="16">
        <el-col :md="12" :span="24">
          <div class="surface-panel" style="padding: 16px">
            <h4 style="margin-top: 0">准备提交的规格</h4>
            <pre class="code-block">{{ formatJson(previewPayload) }}</pre>
          </div>
        </el-col>
        <el-col :md="12" :span="24">
          <div class="surface-panel" style="padding: 16px">
            <h4 style="margin-top: 0">系统解析后的规格</h4>
            <pre class="code-block">{{ formatJson(previewData?.resolved_spec ?? null) }}</pre>
          </div>
        </el-col>
      </el-row>

      <div class="page-actions">
        <el-button type="primary" :loading="previewMutation.isPending.value" @click="previewTask">检查规格</el-button>
        <el-button type="success" :loading="createMutation.isPending.value" @click="createTask">创建任务</el-button>
      </div>
    </div>
  </section>
</template>

<style scoped>
.asset-option {
  display: grid;
  gap: 2px;
  line-height: 1.25;
}
</style>
