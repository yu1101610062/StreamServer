<script setup lang="ts">
import { computed } from "vue";

import PageHeader from "@/shared/components/PageHeader.vue";
import { externalApiDocs } from "@/shared/docs/externalApiDocs";
import { formatJson } from "@/shared/utils/format";

const groupedDocs = computed(() => {
  const groups = new Map<string, typeof externalApiDocs>();
  externalApiDocs.forEach((item) => {
    const list = groups.get(item.category) ?? [];
    list.push(item);
    groups.set(item.category, list);
  });
  return Array.from(groups.entries()).map(([category, items]) => ({
    category,
    items,
  }));
});

function methodTagType(method: string) {
  if (method === "GET") return "info";
  if (method === "POST") return "success";
  if (method === "PUT") return "warning";
  return "primary";
}

function enumText(values?: string[]) {
  return values?.length ? values.join(" / ") : "—";
}

function fieldExampleText(value: unknown) {
  if (value === null || value === undefined) return "—";
  if (typeof value === "string" || typeof value === "number" || typeof value === "boolean") {
    return String(value);
  }
  return formatJson(value);
}

function implementationTagType(owner?: "streamserver" | "business_system") {
  return owner === "business_system" ? "warning" : "success";
}

function implementationLabel(owner?: "streamserver" | "business_system") {
  return owner === "business_system" ? "业务系统实现" : "平台提供";
}
</script>

<template>
  <section class="page-grid">
    <PageHeader
      title="外部 API 文档"
      description="按能力域整理控制台和外部系统可调用的接口，也包含需要业务系统承接的回调接口。每条接口都包含参数说明、请求体字段、响应字段、完整请求示例和响应示例。"
    />

    <div class="metric-grid">
      <div class="surface-card metric-card">
        <div class="subtle">接口总数</div>
        <strong>{{ externalApiDocs.length }}</strong>
      </div>
      <div class="surface-card metric-card">
        <div class="subtle">写接口</div>
        <strong>{{ externalApiDocs.filter((item) => item.method !== "GET").length }}</strong>
      </div>
      <div class="surface-card metric-card">
        <div class="subtle">读接口</div>
        <strong>{{ externalApiDocs.filter((item) => item.method === "GET").length }}</strong>
      </div>
      <div class="surface-card metric-card">
        <div class="subtle">能力域</div>
        <strong>{{ groupedDocs.length }}</strong>
      </div>
    </div>

    <div class="api-doc-groups">
      <section v-for="group in groupedDocs" :key="group.category" class="surface-card api-doc-group">
        <div class="api-doc-group-header">
          <div>
            <div class="page-kicker">API GROUP</div>
            <h3>{{ group.category }}</h3>
            <p class="subtle">这一组接口共 {{ group.items.length }} 条，适合同类场景统一查看。</p>
          </div>
          <div class="api-doc-group-meta">
            <el-tag round>{{ group.items.length }} 个接口</el-tag>
            <el-tag type="primary" round>{{ group.items.filter((item) => item.method !== "GET").length }} 个写接口</el-tag>
          </div>
        </div>

        <el-collapse>
          <el-collapse-item
            v-for="item in group.items"
            :key="`${item.method}-${item.path}`"
            :name="`${item.method}-${item.path}`"
            :title="`${item.method} ${item.path} · ${item.title}`"
          >
            <div class="api-doc-card">
              <div class="section-stack">
                <div class="api-doc-meta-row">
                  <el-tag :type="methodTagType(item.method)" effect="dark">{{ item.method }}</el-tag>
                  <el-tag round>{{ item.successStatus }}</el-tag>
                  <el-tag :type="implementationTagType(item.implementationOwner)" effect="light" round>
                    {{ implementationLabel(item.implementationOwner) }}
                  </el-tag>
                </div>
                <div>
                  <h4>{{ item.summary }}</h4>
                  <p class="subtle">{{ item.description }}</p>
                  <p v-if="item.direction" class="subtle">调用方向：{{ item.direction }}</p>
                </div>
              </div>

              <div class="section-stack">
                <h4>参数说明</h4>
                <div v-if="item.params.length" class="table-scroll">
                  <el-table :data="item.params" size="small">
                    <el-table-column prop="name" label="参数" min-width="150" />
                    <el-table-column prop="location" label="位置" min-width="110" />
                    <el-table-column prop="type" label="类型" min-width="140" />
                    <el-table-column prop="required" label="必填" min-width="100" />
                    <el-table-column label="枚举值" min-width="220">
                      <template #default="{ row }">{{ enumText(row.enumValues) }}</template>
                    </el-table-column>
                    <el-table-column prop="description" label="说明" min-width="320" />
                    <el-table-column label="示例" min-width="220">
                      <template #default="{ row }">{{ fieldExampleText(row.example) }}</template>
                    </el-table-column>
                  </el-table>
                </div>
                <p v-else class="subtle">这条接口没有额外的路径、查询或头部参数要求。</p>
              </div>

              <div class="section-stack">
                <h4>请求体字段</h4>
                <div v-if="item.requestFields?.length" class="table-scroll">
                  <el-table :data="item.requestFields" size="small">
                    <el-table-column prop="path" label="字段路径" min-width="220" />
                    <el-table-column prop="type" label="类型" min-width="140" />
                    <el-table-column prop="required" label="必填" min-width="120" />
                    <el-table-column label="枚举值" min-width="240">
                      <template #default="{ row }">{{ enumText(row.enumValues) }}</template>
                    </el-table-column>
                    <el-table-column prop="description" label="说明" min-width="320" />
                    <el-table-column label="示例" min-width="220">
                      <template #default="{ row }">{{ fieldExampleText(row.example) }}</template>
                    </el-table-column>
                  </el-table>
                </div>
                <p v-else class="subtle">这条接口没有请求体，或请求体为空对象。</p>
              </div>

              <div class="section-stack">
                <h4>响应字段</h4>
                <div v-if="item.responseFields?.length" class="table-scroll">
                  <el-table :data="item.responseFields" size="small">
                    <el-table-column prop="path" label="字段路径" min-width="220" />
                    <el-table-column prop="type" label="类型" min-width="140" />
                    <el-table-column label="枚举值" min-width="240">
                      <template #default="{ row }">{{ enumText(row.enumValues) }}</template>
                    </el-table-column>
                    <el-table-column prop="description" label="说明" min-width="320" />
                    <el-table-column label="示例" min-width="220">
                      <template #default="{ row }">{{ fieldExampleText(row.example) }}</template>
                    </el-table-column>
                  </el-table>
                </div>
                <p v-else class="subtle">这条接口返回空响应体。</p>
              </div>

              <div class="api-doc-example-grid">
                <div class="surface-panel" style="padding: 16px">
                  <h5>请求示例</h5>
                  <pre class="code-block">{{ formatJson(item.requestExample) }}</pre>
                </div>
                <div class="surface-panel" style="padding: 16px">
                  <h5>响应示例</h5>
                  <pre class="code-block">{{ formatJson(item.responseExample) }}</pre>
                </div>
              </div>

              <div v-if="item.notes?.length" class="section-stack">
                <h4>补充说明</h4>
                <ul class="note-list">
                  <li v-for="note in item.notes" :key="note">{{ note }}</li>
                </ul>
              </div>
            </div>
          </el-collapse-item>
        </el-collapse>
      </section>
    </div>
  </section>
</template>
