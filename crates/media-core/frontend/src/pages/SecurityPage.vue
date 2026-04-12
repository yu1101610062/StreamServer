<script setup lang="ts">
import { ref, watch } from "vue";
import { useRouter } from "vue-router";
import { useMutation, useQuery } from "@tanstack/vue-query";
import { ElMessage } from "element-plus";

import { authApi, securityApi } from "@/shared/api/resources";
import PageHeader from "@/shared/components/PageHeader.vue";
import { errorMessage } from "@/shared/utils/format";
import { useSessionStore } from "@/stores/session";

const router = useRouter();
const sessionStore = useSessionStore();

const currentPassword = ref("");
const newPassword = ref("");
const allowlistText = ref("");

const allowlistQuery = useQuery({
  queryKey: ["security", "machine-allowlist"],
  queryFn: () => securityApi.listMachineAllowlist(),
});

watch(
  () => allowlistQuery.data.value,
  (value) => {
    if (!value) return;
    allowlistText.value = value.entries
      .map((entry) => `${entry.cidr}${entry.description ? ` # ${entry.description}` : ""}`)
      .join("\n");
  },
  { immediate: true },
);

const allowlistMutation = useMutation({
  mutationFn: (entries: Array<{ cidr: string; description?: string | null }>) =>
    securityApi.updateMachineAllowlist(entries),
  onSuccess: (result) => {
    allowlistText.value = result.entries
      .map((entry) => `${entry.cidr}${entry.description ? ` # ${entry.description}` : ""}`)
      .join("\n");
  },
  onError: (error) => ElMessage.error(errorMessage(error)),
});

function parseAllowlistText(raw: string) {
  return raw
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => line && !line.startsWith("#"))
    .map((line) => {
      const [cidr, ...rest] = line.split("#");
      return {
        cidr: cidr.trim(),
        description: rest.join("#").trim() || undefined,
      };
    });
}

const changePasswordMutation = useMutation({
  mutationFn: () =>
    authApi.changePassword({
      current_password: currentPassword.value,
      new_password: newPassword.value,
    }),
  onSuccess: async () => {
    ElMessage.success("密码已更新，请重新登录");
    currentPassword.value = "";
    newPassword.value = "";
    await sessionStore.logout();
    await router.push("/login");
  },
  onError: (error) => ElMessage.error(errorMessage(error)),
});

async function saveAllowlist() {
  const entries = parseAllowlistText(allowlistText.value);
  await allowlistMutation.mutateAsync(entries);
  ElMessage.success("机器 API 白名单已更新");
}
</script>

<template>
  <section class="page-grid">
    <PageHeader title="安全设置" description="维护当前账号密码，以及机器 API 白名单。" />

    <div class="metric-grid">
      <div class="surface-card metric-card">
        <div class="subtle">当前账号</div>
        <strong>{{ sessionStore.session?.subject ?? "—" }}</strong>
      </div>
      <div class="surface-card metric-card">
        <div class="subtle">角色</div>
        <strong>{{ sessionStore.session?.role ?? "—" }}</strong>
      </div>
      <div class="surface-card metric-card">
        <div class="subtle">鉴权模式</div>
        <strong>{{ sessionStore.session?.auth_mode ?? "disabled" }}</strong>
      </div>
      <div class="surface-card metric-card">
        <div class="subtle">强制改密</div>
        <strong>{{ sessionStore.session?.must_change_password ? "是" : "否" }}</strong>
      </div>
    </div>

    <div class="surface-card">
      <h3 class="page-section-title">修改当前密码</h3>
      <p class="subtle">提交成功后，当前账号会话会退出，请使用新密码重新登录。</p>
      <el-form label-position="top" @submit.prevent="changePasswordMutation.mutate()">
        <el-form-item label="当前密码">
          <el-input v-model="currentPassword" type="password" show-password />
        </el-form-item>
        <el-form-item label="新密码">
          <el-input v-model="newPassword" type="password" show-password minlength="8" />
        </el-form-item>
        <el-button type="primary" :loading="changePasswordMutation.isPending.value" @click="changePasswordMutation.mutate()">
          更新密码
        </el-button>
      </el-form>
    </div>

    <div class="surface-card">
      <h3 class="page-section-title">机器 API 白名单</h3>
      <p class="subtle">每行一条，格式为 <code>IP/CIDR # 说明</code>。说明可选，以 <code>#</code> 开头的整行会被忽略。</p>
      <el-input
        v-model="allowlistText"
        type="textarea"
        :rows="14"
        placeholder="192.168.1.10/32 # ingest-gateway&#10;10.0.0.0/24 # office-network"
      />
      <div class="page-actions" style="margin-top: 16px">
        <el-button type="primary" :loading="allowlistMutation.isPending.value" @click="saveAllowlist">保存白名单</el-button>
      </div>
    </div>
  </section>
</template>
