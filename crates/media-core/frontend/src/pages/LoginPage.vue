<script setup lang="ts">
import { reactive, ref } from "vue";
import { useRoute, useRouter } from "vue-router";
import { ElMessage } from "element-plus";

import { errorMessage } from "@/shared/utils/format";
import { useSessionStore } from "@/stores/session";

const router = useRouter();
const route = useRoute();
const sessionStore = useSessionStore();

const loginForm = reactive({
  username: "",
  password: "",
});

const bearerToken = ref("");
const loading = ref(false);

function destination() {
  const next = String(route.query.next ?? "/overview");
  return next.startsWith("/") ? next : "/overview";
}

async function submitLogin() {
  loading.value = true;
  try {
    await sessionStore.login(loginForm.username.trim(), loginForm.password);
    ElMessage.success("登录成功");
    await router.push(destination());
  } catch (error) {
    ElMessage.error(errorMessage(error));
  } finally {
    loading.value = false;
  }
}

async function submitBearerToken() {
  loading.value = true;
  try {
    await sessionStore.useBearerToken(bearerToken.value.trim());
    ElMessage.success("Bearer Token 验证通过");
    await router.push(destination());
  } catch (error) {
    ElMessage.error(errorMessage(error));
  } finally {
    loading.value = false;
  }
}
</script>

<template>
  <section class="auth-shell">
    <article class="surface-card auth-card">
      <section class="auth-copy">
        <div class="brand-mark">ACCESS</div>
        <h1>进入 StreamServer</h1>
        <p>
          新控制台已升级为 Vue + TypeScript。登录后可以进入任务、流、录像、节点、调试与外部 API
          文档等全部入口。
        </p>
        <div class="auth-notes">
          <div class="surface-card">
            <strong>本地账号</strong>
            <p class="subtle">
              适用于 <code>local_password</code> 模式。登录成功后会保留 refresh token，用于刷新页面后的会话恢复。
            </p>
          </div>
          <div class="surface-card">
            <strong>Bearer Token</strong>
            <p class="subtle">
              适用于 <code>external_jwt</code> 模式。不会写入本地存储，适合临时会话或第三方签发令牌。
            </p>
          </div>
        </div>
      </section>

      <section class="auth-form">
        <el-tabs>
          <el-tab-pane label="管理员登录">
            <el-form label-position="top" @submit.prevent="submitLogin">
              <el-form-item label="用户名">
                <el-input v-model="loginForm.username" placeholder="admin" autocomplete="username" />
              </el-form-item>
              <el-form-item label="密码">
                <el-input v-model="loginForm.password" type="password" show-password autocomplete="current-password" />
              </el-form-item>
              <el-button type="primary" :loading="loading" @click="submitLogin">登录</el-button>
            </el-form>
          </el-tab-pane>
          <el-tab-pane label="Bearer Token">
            <el-form label-position="top" @submit.prevent="submitBearerToken">
              <el-form-item label="访问令牌">
                <el-input
                  v-model="bearerToken"
                  type="textarea"
                  :autosize="{ minRows: 6, maxRows: 10 }"
                  placeholder="eyJhbGciOi..."
                />
              </el-form-item>
              <el-button type="primary" :loading="loading" @click="submitBearerToken">使用令牌进入</el-button>
            </el-form>
          </el-tab-pane>
        </el-tabs>
      </section>
    </article>
  </section>
</template>
