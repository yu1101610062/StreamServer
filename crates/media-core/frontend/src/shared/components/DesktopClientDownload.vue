<script setup lang="ts">
import { computed, onMounted, ref } from "vue";

import {
  detectDesktopPlatform,
  formatDownloadSize,
  loadDesktopClientManifest,
  platformLabel,
  recommendedDownload,
  type DesktopClientDownload,
} from "@/shared/desktop-downloads";

const loading = ref(false);
const downloads = ref<DesktopClientDownload[]>([]);
const detectedPlatform = detectDesktopPlatform();
const detectedPlatformLabel = platformLabel(detectedPlatform);

const recommended = computed(() => recommendedDownload(downloads.value, detectedPlatform));
const otherDownloads = computed(() =>
  downloads.value.filter((item) => item !== recommended.value),
);

onMounted(async () => {
  loading.value = true;
  try {
    const manifest = await loadDesktopClientManifest();
    downloads.value = manifest.downloads;
  } finally {
    loading.value = false;
  }
});
</script>

<template>
  <div class="desktop-download">
    <div>
      <strong>桌面客户端</strong>
      <p class="subtle">已识别当前系统：{{ detectedPlatformLabel }}</p>
    </div>

    <div v-if="loading" class="subtle">正在读取客户端安装包...</div>
    <template v-else-if="recommended">
      <el-button tag="a" type="primary" :href="recommended.url" download>
        下载 {{ recommended.label }}
        <span v-if="formatDownloadSize(recommended.sizeBytes)">（{{ formatDownloadSize(recommended.sizeBytes) }}）</span>
      </el-button>
      <div v-if="otherDownloads.length > 0" class="download-list">
        <el-link v-for="item in otherDownloads" :key="item.url" :href="item.url" type="primary" download>
          {{ item.label }}
        </el-link>
      </div>
    </template>
    <template v-else-if="downloads.length > 0">
      <div class="download-list">
        <el-link v-for="item in downloads" :key="item.url" :href="item.url" type="primary" download>
          {{ item.label }}
        </el-link>
      </div>
    </template>
    <p v-else class="subtle">当前前端包未内置桌面客户端安装包。</p>
  </div>
</template>

<style scoped>
.desktop-download {
  display: grid;
  gap: 12px;
}

.download-list {
  display: flex;
  flex-wrap: wrap;
  gap: 10px;
}
</style>
