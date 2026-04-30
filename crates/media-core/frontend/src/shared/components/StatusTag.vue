<script setup lang="ts">
import { computed } from "vue";

const props = defineProps<{
  status?: string | null;
}>();

const type = computed(() => {
  switch (props.status) {
    case "RUNNING":
    case "SUCCEEDED":
      return "success";
    case "STARTING":
    case "DISPATCHING":
    case "RECOVERING":
    case "RECLAIMING":
      return "primary";
    case "STOPPING":
      return "warning";
    case "FAILED":
    case "LOST":
      return "danger";
    default:
      return "info";
  }
});
</script>

<template>
  <el-tag :type="type" effect="light" round>{{ status ?? "—" }}</el-tag>
</template>
