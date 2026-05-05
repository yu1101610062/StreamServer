<script setup lang="ts">
import { computed } from "vue";
import { useRoute, useRouter } from "vue-router";

import DesktopClientDownloadMenu from "@/shared/components/DesktopClientDownloadMenu.vue";
import { NAV_ITEMS } from "@/shared/labels";
import { useSessionStore } from "@/stores/session";
import { useThemeStore, type ThemePreference } from "@/stores/theme";

const route = useRoute();
const router = useRouter();
const sessionStore = useSessionStore();
const themeStore = useThemeStore();

const visibleNavItems = computed(() =>
  NAV_ITEMS.filter((item) => sessionStore.hasPermission(item.permission)),
);

async function onLogout() {
  await sessionStore.logout();
  await router.push("/login");
}

function changeTheme(value: ThemePreference) {
  themeStore.setPreference(value);
}
</script>

<template>
  <div class="app-shell">
    <aside class="sidebar">
      <div class="brand">
        <div class="brand-mark">STREAMSERVER</div>
        <strong>控制台</strong>
      </div>
      <nav class="nav-list">
        <RouterLink
          v-for="item in visibleNavItems"
          :key="item.path"
          :to="item.path"
          class="nav-item"
          :class="{ active: route.path === item.path || route.path.startsWith(`${item.path}/`) }"
        >
          <strong>{{ item.label }}</strong>
          <span>{{ item.note }}</span>
        </RouterLink>
      </nav>
    </aside>

    <div class="main-shell">
      <header class="topbar">
        <div>
          <div class="environment-chip">{{ sessionStore.session?.environment ?? "loading" }}</div>
          <div class="topbar-subtitle">
            {{ sessionStore.session?.subject ?? "未登录" }}
            <span v-if="sessionStore.session">· {{ sessionStore.session.role }}</span>
          </div>
        </div>
        <div class="topbar-actions">
          <DesktopClientDownloadMenu />
          <el-select
            :model-value="themeStore.preference"
            class="theme-select"
            size="small"
            @update:model-value="changeTheme"
          >
            <el-option label="跟随系统" value="system" />
            <el-option label="浅色" value="light" />
            <el-option label="深色" value="dark" />
          </el-select>
          <el-button v-if="sessionStore.hasPermission('task_write')" type="primary" plain @click="router.push('/tasks/new')">
            新建任务
          </el-button>
          <el-button @click="onLogout">退出</el-button>
        </div>
      </header>

      <main class="page-shell">
        <slot />
      </main>
    </div>
  </div>
</template>
