<script setup lang="ts">
import { computed } from "vue";

const props = defineProps<{
  url?: string | null;
  label?: string;
}>();

const MEDIA_PROTOCOLS = new Set(["http:", "https:", "rtsp:", "rtmp:", "rtmps:"]);

const safeHref = computed(() => {
  if (!props.url) {
    return null;
  }

  try {
    const url = new URL(props.url);
    return MEDIA_PROTOCOLS.has(url.protocol) ? props.url : null;
  } catch {
    return null;
  }
});

const displayText = computed(() => props.label || props.url || "打开");
</script>

<template>
  <el-link v-if="safeHref" type="primary" :href="safeHref" target="_blank" rel="noreferrer">
    <slot>{{ displayText }}</slot>
  </el-link>
</template>
