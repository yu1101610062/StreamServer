<script setup lang="ts">
import { storeToRefs } from "pinia";
import { useRoute } from "vue-router";

import AppShell from "@/shared/components/AppShell.vue";
import { useSessionStore } from "@/stores/session";

const route = useRoute();
const sessionStore = useSessionStore();
const { loading } = storeToRefs(sessionStore);
const shouldRenderPublicPage = () =>
  Boolean(route.meta.public) && !Boolean(route.meta.shellWhenAuthenticated && sessionStore.isAuthenticated);
</script>

<template>
  <el-config-provider>
    <div v-if="loading" class="auth-shell">
      <div class="surface-card auth-card" style="display: block; max-width: 520px">
        <div class="page-kicker">STREAMSERVER</div>
        <h1>控制台正在启动</h1>
        <p class="subtle">加载会话、页面路由和控制面数据。</p>
      </div>
    </div>
    <RouterView v-else v-slot="{ Component }">
      <component :is="Component" v-if="shouldRenderPublicPage()" />
      <AppShell v-else>
        <component :is="Component" />
      </AppShell>
    </RouterView>
  </el-config-provider>
</template>
