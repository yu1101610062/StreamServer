import "element-plus/dist/index.css";
import "@/styles/theme.css";

import { VueQueryPlugin, QueryClient } from "@tanstack/vue-query";
import ElementPlus from "element-plus";
import { createPinia } from "pinia";
import { createApp } from "vue";

import App from "@/App.vue";
import { router } from "@/router";
import { useSessionStore } from "@/stores/session";
import { useThemeStore } from "@/stores/theme";

const app = createApp(App);
const pinia = createPinia();
const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      retry: 1,
      staleTime: 5_000,
    },
  },
});

app.use(pinia);
app.use(router);
app.use(ElementPlus);
app.use(VueQueryPlugin, { queryClient });

useThemeStore().initialize();
useSessionStore().initialize();

app.mount("#app");
