<script setup lang="ts">
import { Download } from "@element-plus/icons-vue";
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
const platform = detectDesktopPlatform();
const platformName = platformLabel(platform);

const recommended = computed(() => recommendedDownload(downloads.value, platform));

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
  <el-dropdown v-if="downloads.length > 0 || loading" trigger="click">
    <el-button :icon="Download" plain :loading="loading">客户端下载</el-button>
    <template #dropdown>
      <el-dropdown-menu>
        <el-dropdown-item v-if="recommended">
          <a class="download-menu-link" :href="recommended.url" download>
            推荐 {{ recommended.label }}
            <span v-if="formatDownloadSize(recommended.sizeBytes)">（{{ formatDownloadSize(recommended.sizeBytes) }}）</span>
          </a>
        </el-dropdown-item>
        <el-dropdown-item v-else disabled>未识别到 {{ platformName }} 客户端</el-dropdown-item>
        <el-dropdown-item v-for="item in downloads" :key="item.url" divided>
          <a class="download-menu-link" :href="item.url" download>
            {{ item.label }}
            <span v-if="formatDownloadSize(item.sizeBytes)">（{{ formatDownloadSize(item.sizeBytes) }}）</span>
          </a>
        </el-dropdown-item>
      </el-dropdown-menu>
    </template>
  </el-dropdown>
</template>

<style scoped>
.download-menu-link {
  color: inherit;
  text-decoration: none;
  white-space: nowrap;
}
</style>
