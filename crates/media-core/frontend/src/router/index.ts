import { createRouter, createWebHistory } from "vue-router";

import { useSessionStore } from "@/stores/session";
import { routeAccessRedirect } from "@/router/access-guard";

const routes = [
  { path: "/", redirect: "/overview" },
  { path: "/login", name: "login", component: () => import("@/pages/LoginPage.vue"), meta: { public: true } },
  { path: "/overview", name: "overview", component: () => import("@/pages/OverviewPage.vue") },
  {
    path: "/api-docs",
    name: "api-docs",
    component: () => import("@/pages/ApiDocsPage.vue"),
    meta: { public: true, shellWhenAuthenticated: true },
  },
  { path: "/tasks", name: "tasks", component: () => import("@/pages/TasksPage.vue"), meta: { permission: "task_read" } },
  { path: "/tasks/new", name: "task-create", component: () => import("@/pages/TaskCreatePage.vue"), meta: { permission: "task_write" } },
  { path: "/tasks/:id", name: "task-detail", component: () => import("@/pages/TaskDetailPage.vue"), meta: { permission: "task_read" } },
  { path: "/streams", name: "streams", component: () => import("@/pages/StreamsPage.vue"), meta: { permission: "task_read" } },
  { path: "/multicast", name: "multicast", component: () => import("@/pages/MulticastPage.vue"), meta: { permission: "task_read" } },
  { path: "/records", name: "records", component: () => import("@/pages/RecordsPage.vue"), meta: { permission: "record_read" } },
  {
    path: "/file-artifacts",
    name: "file-artifacts",
    component: () => import("@/pages/ArtifactsPage.vue"),
    meta: { permission: "record_read" },
  },
  {
    path: "/media-upload",
    name: "media-upload",
    component: () => import("@/pages/MediaUploadPage.vue"),
    meta: { permission: "task_write" },
  },
  { path: "/security", name: "security", component: () => import("@/pages/SecurityPage.vue"), meta: { permission: "security_write" } },
  { path: "/nodes", redirect: { path: "/overview", query: { focus: "nodes" } } },
  { path: "/debug", name: "debug", component: () => import("@/pages/DebugPage.vue"), meta: { permission: "debug_read" } },
];

export const router = createRouter({
  history: createWebHistory(),
  routes,
});

router.beforeEach(async (to) => {
  const sessionStore = useSessionStore();
  if (sessionStore.loading) {
    await sessionStore.initialize();
  }
  return (
    routeAccessRedirect({
      destinationName: to.name,
      destinationPath: to.fullPath,
      isPublic: Boolean(to.meta.public),
      isAuthenticated: sessionStore.isAuthenticated,
      mustChangePassword: Boolean(sessionStore.session?.must_change_password),
      requiredPermission: to.meta.permission ? String(to.meta.permission) : null,
      permissions: sessionStore.session?.permissions ?? [],
    }) ?? true
  );
});
