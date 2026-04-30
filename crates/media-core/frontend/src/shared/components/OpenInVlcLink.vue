<script setup lang="ts">
import { ElMessage } from "element-plus";
import { computed } from "vue";

import { isDesktopClient, openMediaUrl } from "@/shared/desktop";

const props = defineProps<{
  url?: string | null;
  label?: string;
}>();

const displayText = computed(() => props.label || props.url || "打开");

async function handleClick() {
  if (!props.url) {
    return;
  }

  try {
    const target = await openMediaUrl(props.url);
    if (target === "desktop" && isDesktopClient()) {
      ElMessage.success("已调用 VLC 打开");
    }
  } catch (error) {
    ElMessage.error(error instanceof Error ? error.message : "打开失败");
  }
}
</script>

<template>
  <el-link v-if="url" type="primary" :href="url" target="_blank" rel="noreferrer" @click.prevent="handleClick">
    <slot>{{ displayText }}</slot>
  </el-link>
</template>
