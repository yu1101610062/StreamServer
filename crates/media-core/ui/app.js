const TOKEN_STORAGE_KEY = "streamserver.console.token";

const NAV_ITEMS = [
  {
    path: "/tasks",
    label: "任务中心",
    note: "创建、筛选、派发、重试",
    permission: "task_read",
  },
  {
    path: "/streams",
    label: "流中心",
    note: "在线流、播放地址、关闭流",
    permission: "task_read",
  },
  {
    path: "/multicast",
    label: "组播中心",
    note: "组播任务、网卡、TTL、上下游",
    permission: "task_read",
  },
  {
    path: "/records",
    label: "录像中心",
    note: "录像索引、日期检索、路径复制",
    permission: "record_read",
  },
  {
    path: "/nodes",
    label: "节点中心",
    note: "节点健康、能力矩阵、当前负载",
    permission: "node_read",
  },
  {
    path: "/debug",
    label: "调试台",
    note: "ZLM 原始调试、会话、踢人、关流",
    permission: "debug_read",
  },
];

const TASK_TYPES = [
  { value: "live_relay", label: "live_relay" },
  { value: "file_transcode", label: "file_transcode" },
  { value: "file_to_live", label: "file_to_live" },
  { value: "multicast_bridge", label: "multicast_bridge" },
  { value: "rtp_receive", label: "rtp_receive" },
];

const INPUT_KINDS = [
  "rtsp",
  "rtmp",
  "hls",
  "http_flv",
  "http_ts",
  "file",
  "udp_mpegts_multicast",
  "rtp_multicast",
  "gb_rtp",
];

const PUBLISH_KINDS = ["file", "zlm_ingest", "udp_mpegts_multicast", "rtp_multicast"];
const START_MODES = ["immediate", "manual", "cron", "at"];
const RECORD_FORMATS = ["mp4", "hls", "both"];
const RECOVERY_POLICIES = ["never", "on_failure", "always"];
const PROFILE_OPTIONS = [
  "",
  "realtime_compat",
  "rtc_web_compat",
  "archive_quality",
  "multicast_ts",
  "rtmp_hevc_ext",
];
const AUTO_REFRESH_MS = 10000;

const STATUS_THEME = {
  RUNNING: "status-running",
  STARTING: "status-starting",
  DISPATCHING: "status-dispatching",
  RECOVERING: "status-recovering",
  STOPPING: "status-stopping",
  FAILED: "status-failed",
  LOST: "status-lost",
  CREATED: "status-created",
  VALIDATING: "status-validating",
  QUEUED: "status-queued",
  SUCCEEDED: "status-succeeded status-outline",
  CANCELED: "status-canceled status-outline",
};

const state = {
  token: window.localStorage.getItem(TOKEN_STORAGE_KEY) || "",
  session: null,
  sessionError: null,
  route: parseRoute(window.location.pathname, window.location.search),
  routeData: null,
  pageError: null,
  loading: false,
  toasts: [],
  cache: {
    taskDetails: new Map(),
    templates: null,
    templateDetails: new Map(),
    nodes: null,
    nodeInsights: new Map(),
  },
  ui: {
    authModalOpen: false,
    createOpen: false,
    openNodeId: "",
    createStep: 1,
    createDraft: createDefaultDraft(),
    createPreview: null,
    createError: null,
    authDraftToken: "",
    debug: {
      nodeId: "",
      mediaResult: null,
      sessionsResult: null,
      playersResult: null,
      statisticResult: null,
      threadsLoadResult: null,
      workThreadsLoadResult: null,
      snapResult: null,
      hooksResult: null,
      lastError: null,
    },
  },
};
let autoRefreshTimer = null;

const appRoot = document.getElementById("app");

boot().catch((error) => {
  console.error(error);
  appRoot.innerHTML = renderFatal(error);
});

async function boot() {
  window.addEventListener("popstate", async () => {
    state.route = parseRoute(window.location.pathname, window.location.search);
    await refreshRoute();
  });
  document.addEventListener("click", handleClick);
  document.addEventListener("submit", handleSubmit);
  document.addEventListener("change", handleChange);
  document.addEventListener("input", handleInput);
  startAutoRefresh();
  await refreshSession(true);
  await refreshRoute();
}

function startAutoRefresh() {
  if (autoRefreshTimer) {
    window.clearInterval(autoRefreshTimer);
  }
  autoRefreshTimer = window.setInterval(async () => {
    if (document.hidden || state.loading || state.ui.authModalOpen || state.ui.createOpen) {
      return;
    }
    try {
      await refreshSession(true);
      await refreshRoute();
    } catch (error) {
      console.error(error);
    }
  }, AUTO_REFRESH_MS);
}

async function refreshSession(silent) {
  try {
    state.session = await apiRequest("/api/v1/me");
    state.sessionError = null;
  } catch (error) {
    state.session = null;
    state.sessionError = error;
    if (!silent && !isAuthError(error)) {
      toast(errorMessage(error), "error");
    }
  }
}

async function refreshRoute() {
  state.loading = true;
  state.pageError = null;
  renderApp();
  try {
    state.routeData = await loadRouteData(state.route);
  } catch (error) {
    state.routeData = null;
    state.pageError = error;
  }
  state.loading = false;
  renderApp();
}

function renderApp() {
  appRoot.className = "app-shell";
  appRoot.innerHTML = `
    ${renderSidebar()}
    <main class="main-panel">
      ${renderTopbar()}
      <section class="page-body">
        ${renderPageBody()}
      </section>
    </main>
    ${renderCreateDrawer()}
    ${renderAuthModal()}
    ${renderToasts()}
  `;
}

function renderSidebar() {
  const visibleItems = NAV_ITEMS.filter((item) => canAccess(item.permission));
  return `
    <aside class="sidebar">
      <div class="brand">
        <div class="brand-mark">CONTROL PLANE</div>
        <div>
          <h1>StreamServer</h1>
          <p>任务、节点、流和调试入口都由 <code>media-core</code> 单点托管。</p>
        </div>
      </div>
      <section class="session-card">
        <strong>${escapeHtml(state.session?.subject || "未认证会话")}</strong>
        <span class="muted">${escapeHtml(sessionSubtitle())}</span>
        <div class="toolbar-actions">
          ${state.session ? renderRolePill(state.session.role) : ""}
          <button class="ghost-button" data-action="open-auth-modal">Token</button>
        </div>
      </section>
      <nav class="sidebar-nav">
        ${visibleItems
          .map(
            (item, index) => `
              <a class="nav-item ${state.route.path.startsWith(item.path) ? "active" : ""}" href="${item.path}" data-link>
                <span>
                  <strong>${escapeHtml(item.label)}</strong>
                  <small>${escapeHtml(item.note)}</small>
                </span>
                <span class="nav-badge">${index + 1}</span>
              </a>
            `,
          )
          .join("")}
      </nav>
    </aside>
  `;
}

function renderTopbar() {
  const title = currentRouteTitle();
  const subtitle = state.pageError
    ? "页面加载失败"
    : state.loading
      ? "正在读取控制面数据"
      : currentRouteSubtitle();
  return `
    <header class="topbar">
      <div>
        <h2>${escapeHtml(title)}</h2>
        <p>${escapeHtml(subtitle)}</p>
      </div>
      <div class="topbar-actions">
        <span class="tag">${escapeHtml(state.session?.environment || "unknown")}</span>
        <button class="ghost-button" data-action="refresh-page">刷新</button>
        ${canAccess("task_write") ? `<button class="button" data-action="open-create-drawer">新建任务</button>` : ""}
      </div>
    </header>
  `;
}

function renderPageBody() {
  if (state.loading) {
    return renderLoadingPanel();
  }
  if (state.pageError) {
    if (shouldRenderAuthRequired(state.pageError)) {
      return renderAuthRequired();
    }
    return renderErrorPanel("页面加载失败", errorMessage(state.pageError));
  }
  if (!state.routeData) {
    return renderEmptyState("暂无内容", "当前页面还没有可展示的数据。");
  }
  return renderRouteBody(state.route, state.routeData);
}

async function loadRouteData(route) {
  if (state.sessionError && isAuthError(state.sessionError)) {
    return { authRequired: true };
  }
  switch (route.name) {
    case "tasks":
      return await loadTasksData(route);
    case "task-detail":
      return await loadTaskDetailData(route);
    case "streams":
      return await loadStreamsData(route);
    case "multicast":
      return await loadMulticastData(route);
    case "records":
      return await loadRecordsData(route);
    case "nodes":
      return await loadNodesData(route);
    case "debug":
      return await loadDebugData(route);
    default:
      return await loadTasksData(route);
  }
}

function renderRouteBody(route, data) {
  if (data.authRequired) {
    return renderAuthRequired();
  }
  switch (route.name) {
    case "tasks":
      return renderTasksPage(data);
    case "task-detail":
      return renderTaskDetailPage(route, data);
    case "streams":
      return renderStreamsPage(data);
    case "multicast":
      return renderMulticastPage(data);
    case "records":
      return renderRecordsPage(data);
    case "nodes":
      return renderNodesPage(data);
    case "debug":
      return renderDebugPage(data);
    default:
      return renderTasksPage(data);
  }
}

async function loadTasksData(route) {
  const params = route.searchParams;
  const query = new URLSearchParams();
  copyIfPresent(params, query, ["status", "type", "assigned_node_id", "keyword", "created_from", "created_to", "page", "page_size", "sort_by", "sort_order"]);
  if (!query.get("page_size")) {
    query.set("page_size", "20");
  }
  const [tasksPage, nodes, templates] = await Promise.all([
    apiRequest(`/api/v1/tasks?${query.toString()}`),
    canAccess("node_read") ? fetchNodesCached(false) : Promise.resolve([]),
    canAccess("template_read") ? fetchTemplatesCached(false) : Promise.resolve([]),
  ]);
  return { tasksPage, nodes, templates };
}

async function loadTaskDetailData(route) {
  const taskId = route.params.id;
  const params = route.searchParams;
  const tab = params.get("tab") || "overview";
  const detail = await fetchTaskDetail(taskId, true);
  const [recordsPage, streams] = await Promise.all([
    canAccess("record_read")
      ? apiRequest(`/api/v1/records?task_id=${encodeURIComponent(taskId)}&page_size=5`)
      : Promise.resolve({ items: [], page: 1, page_size: 5, total: 0 }),
    canAccess("task_read")
      ? apiRequest(`/api/v1/streams?task_id=${encodeURIComponent(taskId)}`)
      : Promise.resolve([]),
  ]);

  const eventParams = new URLSearchParams();
  copyIfPresent(params, eventParams, ["attempt_no", "source", "event_type", "page", "page_size"]);
  if (!eventParams.get("page_size")) {
    eventParams.set("page_size", "20");
  }

  const logParams = new URLSearchParams();
  copyIfPresent(params, logParams, ["log_attempt_no", "log_stream", "log_cursor", "log_limit"]);
  if (!logParams.get("limit") && params.get("log_limit")) {
    logParams.set("limit", params.get("log_limit"));
  }
  if (!logParams.get("limit")) {
    logParams.set("limit", "200");
  }
  if (params.get("log_stream")) {
    logParams.set("stream", params.get("log_stream"));
  }
  if (params.get("log_attempt_no")) {
    logParams.set("attempt_no", params.get("log_attempt_no"));
  }
  if (params.get("log_cursor")) {
    logParams.set("cursor", params.get("log_cursor"));
  }

  const [eventsPage, logs] = await Promise.all([
    apiRequest(`/api/v1/tasks/${taskId}/events?${eventParams.toString()}`),
    apiRequest(`/api/v1/tasks/${taskId}/logs?${logParams.toString()}`),
  ]);

  return { detail, recordsPage, streams, eventsPage, logs, activeTab: tab };
}

async function loadStreamsData(route) {
  const params = route.searchParams;
  const query = new URLSearchParams();
  copyIfPresent(params, query, ["schema", "app", "stream", "node_id", "has_viewer", "task_id"]);
  const [streams, nodes] = await Promise.all([
    apiRequest(`/api/v1/streams?${query.toString()}`),
    canAccess("node_read") ? fetchNodesCached(false) : Promise.resolve([]),
  ]);

  const taskDetails = new Map();
  await Promise.all(
    [...new Set(streams.map((stream) => stream.task_id))]
      .slice(0, 30)
      .map(async (taskId) => {
        taskDetails.set(taskId, await fetchTaskDetail(taskId, false));
      }),
  );

  return { streams, nodes, taskDetails };
}

async function loadMulticastData(route) {
  const params = route.searchParams;
  const query = new URLSearchParams();
  query.set("type", "multicast_bridge");
  query.set("page_size", params.get("page_size") || "100");
  if (params.get("status")) {
    query.set("status", params.get("status"));
  }
  const [tasksPage, nodes, streams] = await Promise.all([
    apiRequest(`/api/v1/tasks?${query.toString()}`),
    canAccess("node_read") ? fetchNodesCached(false) : Promise.resolve([]),
    canAccess("task_read") ? apiRequest("/api/v1/streams") : Promise.resolve([]),
  ]);
  const taskDetails = new Map();
  await Promise.all(
    tasksPage.items.map(async (task) => {
      taskDetails.set(task.id, await fetchTaskDetail(task.id, false));
    }),
  );
  const streamsByTask = new Map();
  (streams || []).forEach((stream) => {
    if (!streamsByTask.has(stream.task_id)) {
      streamsByTask.set(stream.task_id, []);
    }
    streamsByTask.get(stream.task_id).push(stream);
  });
  return { tasksPage, nodes, taskDetails, streamsByTask };
}

async function loadRecordsData(route) {
  const params = route.searchParams;
  const query = new URLSearchParams();
  copyIfPresent(params, query, ["task_id", "stream", "date_from", "date_to", "page", "page_size"]);
  if (!query.get("page_size")) {
    query.set("page_size", "20");
  }
  const recordsPage = await apiRequest(`/api/v1/records?${query.toString()}`);
  return { recordsPage };
}

async function loadNodesData() {
  const nodes = await fetchNodesCached(true);
  if (state.ui.openNodeId) {
    state.cache.nodeInsights.set(state.ui.openNodeId, await loadNodeInsight(state.ui.openNodeId));
  }
  return { nodes };
}

async function loadDebugData() {
  const nodes = await fetchNodesCached(false);
  if (!state.ui.debug.nodeId && nodes.length > 0) {
    state.ui.debug.nodeId = nodes[0].id;
  }
  return { nodes };
}

function renderTasksPage(data) {
  const params = state.route.searchParams;
  const nodeOptions = data.nodes || [];
  const templateLookup = new Map((data.templates || []).map((template) => [template.id, template.name]));
  return `
    <section class="hero-panel">
      <div class="section-header">
        <div>
          <div class="brand-mark">TASKS</div>
          <h3>任务中心</h3>
          <p>筛选、排序、启动、停止、重试、克隆，并从同一控制台进入详情和调试。</p>
        </div>
        <div class="section-actions">
          ${canAccess("task_write") ? `<button class="button" data-action="open-create-drawer">新建任务</button>` : ""}
        </div>
      </div>
      <form id="tasks-filter-form" class="filters">
        ${renderSelectField("状态", "status", ["", "CREATED", "VALIDATING", "QUEUED", "DISPATCHING", "STARTING", "RUNNING", "STOPPING", "RECOVERING", "SUCCEEDED", "FAILED", "CANCELED", "LOST"], params.get("status") || "")}
        ${renderSelectField("类型", "type", ["", ...TASK_TYPES.map((item) => item.value)], params.get("type") || "")}
        ${renderSelectField("节点", "assigned_node_id", ["", ...nodeOptions.map((node) => node.id)], params.get("assigned_node_id") || "", (value) => value === "" ? "全部节点" : nodeLabel(nodeOptions.find((node) => node.id === value)))}
        ${renderTextField("关键字", "keyword", params.get("keyword") || "", "name / task_id")}
        ${renderDateTimeField("创建开始", "created_from", params.get("created_from") || "")}
        ${renderDateTimeField("创建结束", "created_to", params.get("created_to") || "")}
        ${renderSelectField("排序字段", "sort_by", ["", "created_at", "updated_at", "priority", "status"], params.get("sort_by") || "", (value) => value || "默认")}
        ${renderSelectField("排序方向", "sort_order", ["", "asc", "desc"], params.get("sort_order") || "", (value) => value || "默认")}
        <div class="toolbar-actions">
          <button class="button" type="submit">应用筛选</button>
          <button class="ghost-button" type="button" data-action="reset-task-filters">重置</button>
        </div>
      </form>
    </section>
    <section class="table-panel">
      <div class="table-toolbar">
        <div>
          <h3>任务列表</h3>
          <p>共 ${data.tasksPage.total} 条，当前第 ${data.tasksPage.page} 页。</p>
        </div>
        <div class="toolbar-actions">
          ${renderPager("tasks", data.tasksPage)}
        </div>
      </div>
      <div class="table-wrap">
        <table>
          <thead>
            <tr>
              <th>Task ID</th>
              <th>名称</th>
              <th>类型</th>
              <th>状态</th>
              <th>优先级</th>
              <th>节点</th>
              <th>模板</th>
              <th>创建人</th>
              <th>创建时间</th>
              <th>更新时间</th>
              <th>操作</th>
            </tr>
          </thead>
          <tbody>
            ${
              data.tasksPage.items.length
                ? data.tasksPage.items
                    .map((task) => {
                      const node = nodeOptions.find((item) => item.id === task.assigned_node_id);
                      return `
                        <tr>
                          <td><a href="/tasks/${task.id}" data-link class="mono">${shortId(task.id)}</a></td>
                          <td>
                            <strong>${escapeHtml(task.name)}</strong>
                            <div class="subtle">attempt ${task.current_attempt_no || 0}</div>
                          </td>
                          <td><span class="tag">${escapeHtml(task.type)}</span></td>
                          <td>${statusPill(task.status)}</td>
                          <td>${escapeHtml(String(task.priority))}</td>
                          <td>${escapeHtml(nodeLabel(node))}</td>
                          <td>${escapeHtml(templateLookup.get(task.template_id) || task.template_id || "—")}</td>
                          <td>${escapeHtml(task.created_by || "—")}</td>
                          <td>${escapeHtml(formatTime(task.created_at))}</td>
                          <td>${escapeHtml(formatTime(task.updated_at))}</td>
                          <td>${renderTaskActions(task)}</td>
                        </tr>
                      `;
                    })
                    .join("")
                : `<tr><td colspan="11">${renderInlineEmpty("没有命中条件的任务。")}</td></tr>`
            }
          </tbody>
        </table>
      </div>
    </section>
  `;
}

function renderTaskDetailPage(route, data) {
  const detail = data.detail;
  const task = detail.task;
  const params = state.route.searchParams;
  const activeTab = data.activeTab;
  const lastIssue = deriveLastIssue(detail.recent_events);
  const diffPaths = computeDiffPaths(detail.requested_spec, detail.resolved_spec || {});
  return `
    <section class="hero-panel">
      <div class="section-header">
        <div>
          <div class="brand-mark">TASK DETAIL</div>
          <h3>${escapeHtml(task.name)}</h3>
          <p>${escapeHtml(task.id)} · ${escapeHtml(task.type)} · 当前 Attempt ${escapeHtml(String(task.current_attempt_no || 0))}</p>
        </div>
        <div class="section-actions">
          ${statusPill(task.status)}
          ${renderTaskActions(task, true)}
        </div>
      </div>
      <div class="overview-grid">
        ${metricCard("当前状态", statusPill(task.status), true)}
        ${metricCard("执行节点", task.assigned_node_id || "未分配")}
        ${metricCard("最近错误", lastIssue || "—")}
        ${metricCard("录像摘要", `${data.recordsPage.total} 条文件记录`)}
        ${metricCard("流绑定摘要", `${data.streams.length} 条流绑定`)}
        ${metricCard("规格差异", `${diffPaths.length} 个差异路径`)}
      </div>
    </section>
    <section class="panel">
      <div class="panel-header">
        <div>
          <h3>详情页签</h3>
          <p>概览、事件、日志、requested_spec、resolved_spec。</p>
        </div>
        <div class="tabs">
          ${renderTaskDetailTab(route.params.id, activeTab, "overview", "概览")}
          ${renderTaskDetailTab(route.params.id, activeTab, "events", "事件")}
          ${renderTaskDetailTab(route.params.id, activeTab, "logs", "日志")}
          ${renderTaskDetailTab(route.params.id, activeTab, "requested", "requested_spec")}
          ${renderTaskDetailTab(route.params.id, activeTab, "resolved", "resolved_spec")}
        </div>
      </div>
      ${
        activeTab === "overview"
          ? renderTaskOverview(detail, data.recordsPage, data.streams, diffPaths)
          : activeTab === "events"
            ? renderTaskEventsTab(route.params.id, data.eventsPage)
            : activeTab === "logs"
              ? renderTaskLogsTab(route.params.id, data.logs, params)
              : activeTab === "requested"
                ? `<pre class="json-block">${escapeHtml(JSON.stringify(detail.requested_spec, null, 2))}</pre>`
                : `<pre class="json-block">${escapeHtml(JSON.stringify(detail.resolved_spec || {}, null, 2))}</pre>`
      }
    </section>
  `;
}

function renderStreamsPage(data) {
  const params = state.route.searchParams;
  const nodeMap = new Map((data.nodes || []).map((node) => [node.id, node]));
  return `
    <section class="hero-panel">
      <div class="section-header">
        <div>
          <div class="brand-mark">STREAMS</div>
          <h3>流中心</h3>
          <p>在线流、播放地址、关联任务、viewer 状态，以及管理员关流操作。</p>
        </div>
      </div>
      <form id="streams-filter-form" class="filters">
        ${renderTextField("Schema", "schema", params.get("schema") || "", "rtsp / rtmp / http")}
        ${renderTextField("App", "app", params.get("app") || "", "live")}
        ${renderTextField("Stream", "stream", params.get("stream") || "", "camera01")}
        ${renderTextField("Task ID", "task_id", params.get("task_id") || "", "可选")}
        ${renderSelectField("节点", "node_id", ["", ...(data.nodes || []).map((node) => node.id)], params.get("node_id") || "", (value) => value === "" ? "全部节点" : nodeLabel(nodeMap.get(value)))}
        ${renderSelectField("有观众", "has_viewer", ["", "true", "false"], params.get("has_viewer") || "", (value) => value === "" ? "全部" : value)}
        <div class="toolbar-actions">
          <button class="button" type="submit">筛选</button>
          <button class="ghost-button" type="button" data-action="reset-stream-filters">重置</button>
        </div>
      </form>
    </section>
    <section class="table-panel">
      <div class="table-toolbar">
        <div>
          <h3>在线流</h3>
          <p>共 ${data.streams.length} 条。</p>
        </div>
      </div>
      <div class="table-wrap">
        <table>
          <thead>
            <tr>
              <th>Schema</th>
              <th>Vhost/App/Stream</th>
              <th>Task</th>
              <th>Node</th>
              <th>Viewer</th>
              <th>Recording</th>
              <th>Play URLs</th>
              <th>操作</th>
            </tr>
          </thead>
          <tbody>
            ${
              data.streams.length
                ? data.streams
                    .map((stream) => {
                      const task = data.taskDetails.get(stream.task_id);
                      const node = nodeMap.get(stream.node_id);
                      return `
                        <tr>
                          <td><span class="tag">${escapeHtml(stream.schema)}</span></td>
                          <td>
                            <strong>${escapeHtml(stream.vhost)}</strong>
                            <div class="mono">${escapeHtml(`${stream.app}/${stream.stream}`)}</div>
                          </td>
                          <td><a href="/tasks/${stream.task_id}" data-link class="mono">${shortId(stream.task_id)}</a></td>
                          <td>${escapeHtml(nodeLabel(node))}</td>
                          <td>${escapeHtml(viewerCountLabel(stream.viewer_count, stream.has_viewer))}</td>
                          <td>${escapeHtml(renderRecordingLabel(task))}</td>
                          <td>${renderPlayUrls(stream.play_urls || [])}</td>
                          <td>
                            <div class="toolbar-actions">
                              <a class="ghost-button" href="/tasks/${stream.task_id}" data-link>任务</a>
                              ${canAccess("debug_read") && stream.node_id ? `<button class="danger-button" data-action="close-stream" data-node-id="${stream.node_id}" data-schema="${escapeAttr(stream.schema)}" data-vhost="${escapeAttr(stream.vhost)}" data-app="${escapeAttr(stream.app)}" data-stream="${escapeAttr(stream.stream)}">关流</button>` : ""}
                            </div>
                          </td>
                        </tr>
                      `;
                    })
                    .join("")
                : `<tr><td colspan="8">${renderInlineEmpty("当前没有在线流。")}</td></tr>`
            }
          </tbody>
        </table>
      </div>
    </section>
  `;
}

function renderMulticastPage(data) {
  const nodeMap = new Map((data.nodes || []).map((node) => [node.id, node]));
  return `
    <section class="hero-panel">
      <div class="section-header">
        <div>
          <div class="brand-mark">MULTICAST</div>
          <h3>组播中心</h3>
          <p>集中查看组播任务、网卡、TTL、上下游，以及最近错误。</p>
        </div>
      </div>
    </section>
    <section class="table-panel">
      <div class="table-toolbar">
        <div>
          <h3>组播任务</h3>
          <p>共 ${data.tasksPage.total} 条 multicast_bridge 任务。</p>
        </div>
      </div>
      <div class="table-wrap">
        <table>
          <thead>
            <tr>
              <th>Task</th>
              <th>Mode</th>
              <th>Group</th>
              <th>Port</th>
              <th>Interface</th>
              <th>TTL</th>
              <th>Node</th>
              <th>Status</th>
              <th>额外信息</th>
            </tr>
          </thead>
          <tbody>
            ${
              data.tasksPage.items.length
                ? data.tasksPage.items
                    .map((task) => {
                      const detail = data.taskDetails.get(task.id);
                      const spec = detail?.resolved_spec || {};
                      const row = multicastRowModel(
                        task,
                        spec,
                        detail,
                        nodeMap.get(task.assigned_node_id),
                        data.streamsByTask.get(task.id) || [],
                      );
                      return `
                        <tr>
                          <td><a href="/tasks/${task.id}" data-link class="mono">${shortId(task.id)}</a></td>
                          <td>${escapeHtml(row.mode)}</td>
                          <td>${escapeHtml(row.group)}</td>
                          <td>${escapeHtml(row.port)}</td>
                          <td>${escapeHtml(row.interfaceIp)}</td>
                          <td>${escapeHtml(row.ttl)}</td>
                          <td>${escapeHtml(row.node)}</td>
                          <td>${statusPill(task.status)}</td>
                          <td>
                            <div class="subtle">最近码率: ${escapeHtml(row.bitrate)}</div>
                            <div class="subtle">最近错误: ${escapeHtml(row.lastError)}</div>
                            <div class="subtle">上下游: ${escapeHtml(row.binding)}</div>
                          </td>
                        </tr>
                      `;
                    })
                    .join("")
                : `<tr><td colspan="9">${renderInlineEmpty("当前没有 multicast_bridge 任务。")}</td></tr>`
            }
          </tbody>
        </table>
      </div>
    </section>
  `;
}

function renderRecordsPage(data) {
  const params = state.route.searchParams;
  return `
    <section class="hero-panel">
      <div class="section-header">
        <div>
          <div class="brand-mark">RECORDS</div>
          <h3>录像中心</h3>
          <p>按照日期、任务和流名检索录像，并直接复制路径或跳转任务。</p>
        </div>
      </div>
      <form id="records-filter-form" class="filters">
        ${renderTextField("Task ID", "task_id", params.get("task_id") || "", "uuid")}
        ${renderTextField("Stream", "stream", params.get("stream") || "", "camera01")}
        ${renderDateTimeField("开始时间", "date_from", params.get("date_from") || "")}
        ${renderDateTimeField("结束时间", "date_to", params.get("date_to") || "")}
        <div class="toolbar-actions">
          <button class="button" type="submit">筛选</button>
          <button class="ghost-button" type="button" data-action="reset-record-filters">重置</button>
        </div>
      </form>
    </section>
    <section class="table-panel">
      <div class="table-toolbar">
        <div>
          <h3>录像文件</h3>
          <p>共 ${data.recordsPage.total} 条，当前第 ${data.recordsPage.page} 页。</p>
        </div>
        <div class="toolbar-actions">
          ${renderPager("records", data.recordsPage)}
        </div>
      </div>
      <div class="table-wrap">
        <table>
          <thead>
            <tr>
              <th>Record ID</th>
              <th>Task</th>
              <th>Stream</th>
              <th>File Path</th>
              <th>Size</th>
              <th>时长</th>
              <th>开始时间</th>
              <th>Source</th>
              <th>操作</th>
            </tr>
          </thead>
          <tbody>
            ${
              data.recordsPage.items.length
                ? data.recordsPage.items
                    .map(
                      (record) => `
                        <tr>
                          <td class="mono">${shortId(record.id)}</td>
                          <td><a href="/tasks/${record.task_id}" data-link class="mono">${shortId(record.task_id)}</a></td>
                          <td>${escapeHtml([record.vhost, record.app, record.stream].filter(Boolean).join("/") || "—")}</td>
                          <td class="mono">${escapeHtml(record.file_path)}</td>
                          <td>${escapeHtml(formatBytes(record.file_size))}</td>
                          <td>${escapeHtml(record.time_len ? `${record.time_len}s` : "—")}</td>
                          <td>${escapeHtml(formatTime(record.start_time || record.created_at))}</td>
                          <td>${escapeHtml(record.source)}</td>
                          <td>
                            <div class="toolbar-actions">
                              <button class="ghost-button" data-action="copy" data-value="${escapeAttr(record.file_path)}">复制路径</button>
                              <a class="ghost-button" href="/tasks/${record.task_id}" data-link>任务</a>
                            </div>
                          </td>
                        </tr>
                      `,
                    )
                    .join("")
                : `<tr><td colspan="9">${renderInlineEmpty("当前没有录像文件。")}</td></tr>`
            }
          </tbody>
        </table>
      </div>
    </section>
  `;
}

function renderNodesPage(data) {
  return `
    <section class="hero-panel">
      <div class="section-header">
        <div>
          <div class="brand-mark">NODES</div>
          <h3>节点中心</h3>
          <p>查看节点健康、能力矩阵、实时负载和 ZLM 概览。</p>
        </div>
      </div>
      <div class="metric-grid">
        ${data.nodes.map((node) => renderNodeMetric(node)).join("") || renderInlineEmpty("暂无节点。")}
      </div>
    </section>
    <section class="panel">
      <div class="panel-header">
        <div>
          <h3>节点明细</h3>
          <p>展开单个节点可查看能力矩阵、当前任务和 ZLM 概览。</p>
        </div>
      </div>
      <div class="node-detail-grid">
        ${
          data.nodes.length
            ? data.nodes
                .map((node) => {
                  const insight = state.cache.nodeInsights.get(node.id);
                  const open = state.ui.openNodeId === node.id;
                  return `
                    <article class="node-detail-card">
                      <div class="section-header">
                        <div>
                          <h3>${escapeHtml(node.node_name)}</h3>
                          <p>${escapeHtml(node.hostname)} · ${escapeHtml(node.network_mode)} · ${escapeHtml(node.id)}</p>
                        </div>
                        <div class="section-actions">
                          ${node.healthy ? `<span class="pill status-running">healthy</span>` : `<span class="pill status-failed">unhealthy</span>`}
                          <button class="ghost-button" data-action="toggle-node-detail" data-node-id="${node.id}">${open ? "收起" : "展开"}</button>
                          <a class="ghost-button" href="/tasks?assigned_node_id=${node.id}" data-link>任务</a>
                        </div>
                      </div>
                      ${
                        open
                          ? renderExpandedNodeInsight(node, insight)
                          : `<div class="subtle">上次心跳: ${escapeHtml(formatTime(node.last_seen_at))} · CPU ${formatPercent(node.cpu_percent)} · MEM ${formatPercent(node.mem_percent)} · 运行任务 ${escapeHtml(String(node.running_tasks ?? 0))}</div>`
                      }
                    </article>
                  `;
                })
                .join("")
            : renderInlineEmpty("暂无节点明细。")
        }
      </div>
    </section>
  `;
}

function renderDebugPage(data) {
  const selectedNode = data.nodes.find((node) => node.id === state.ui.debug.nodeId);
  return `
    <section class="hero-panel">
      <div class="section-header">
        <div>
          <div class="brand-mark">DEBUG</div>
          <h3>调试台</h3>
          <p>管理员专用。封装 ZLM 媒体列表、Session、玩家列表、踢会话和关流。</p>
        </div>
      </div>
      <div class="form-grid">
        ${renderSelectField("节点", "debug-node-id", ["", ...data.nodes.map((node) => node.id)], state.ui.debug.nodeId || "", (value) => value === "" ? "请选择节点" : nodeLabel(data.nodes.find((node) => node.id === value)), true)}
      </div>
      ${
        selectedNode
          ? `<p class="subtle">当前节点: ${escapeHtml(selectedNode.node_name)} · ${escapeHtml(selectedNode.zlm_version || "unknown ZLM")}</p>`
          : `<p class="subtle">先选择一个节点，再执行调试查询。</p>`
      }
    </section>
    <section class="debug-grid">
      <div class="panel">
        <div class="panel-header">
          <div>
            <h3>ZLM 统计</h3>
            <p>对象统计、前台线程负载和后台线程负载。</p>
          </div>
        </div>
        <div class="toolbar-actions">
          <button class="button" data-action="debug-load-statistic">加载统计</button>
        </div>
        <div class="split-grid">
          <div>
            <h4>getStatistic</h4>
            ${renderDebugResult(state.ui.debug.statisticResult)}
          </div>
          <div>
            <h4>Threads / WorkThreads</h4>
            ${renderThreadLoadPanel(state.ui.debug.threadsLoadResult, state.ui.debug.workThreadsLoadResult)}
          </div>
        </div>
      </div>
      <div class="panel">
        <div class="panel-header">
          <div>
            <h3>媒体列表</h3>
            <p>按 schema / vhost / app / stream 查询。</p>
          </div>
        </div>
        <form id="debug-media-form" class="form-grid">
          ${renderTextField("Schema", "schema", "", "rtsp / rtmp")}
          ${renderTextField("Vhost", "vhost", "", "__defaultVhost__")}
          ${renderTextField("App", "app", "", "live")}
          ${renderTextField("Stream", "stream", "", "camera01")}
          <div class="toolbar-actions">
            <button class="button" type="submit">查询媒体</button>
          </div>
        </form>
        ${renderDebugResult(state.ui.debug.mediaResult)}
      </div>
      <div class="panel">
        <div class="panel-header">
          <div>
            <h3>Session 与玩家</h3>
            <p>读取 getAllSession 与 getMediaPlayerList。</p>
          </div>
        </div>
        <div class="toolbar-actions">
          <button class="button" data-action="debug-load-sessions">查询 Session</button>
          <button class="soft-button" data-action="debug-load-players">查询玩家</button>
        </div>
        <div class="split-grid">
          <div>
            <h4>Session</h4>
            ${renderDebugResult(state.ui.debug.sessionsResult)}
          </div>
          <div>
            <h4>Players</h4>
            ${renderDebugResult(state.ui.debug.playersResult)}
          </div>
        </div>
      </div>
      <div class="panel">
        <div class="panel-header">
          <div>
            <h3>执行动作</h3>
            <p>单个踢会话、批量踢会话、主动关流和截图。</p>
          </div>
        </div>
        <form id="debug-kick-form" class="form-grid">
          ${renderTextField("Session ID", "session_id", "", "必填")}
          <div class="toolbar-actions">
            <button class="danger-button" type="submit">踢会话</button>
          </div>
        </form>
        <form id="debug-kick-batch-form" class="form-grid">
          ${renderTextField("Local Port", "local_port", "", "例如 554")}
          ${renderTextField("Peer IP", "peer_ip", "", "例如 10.0.0.8")}
          <div class="toolbar-actions">
            <button class="danger-button" type="submit">批量踢会话</button>
          </div>
        </form>
        <form id="debug-close-form" class="form-grid">
          ${renderTextField("Schema", "schema", "", "rtsp / rtmp / http")}
          ${renderTextField("Vhost", "vhost", "", "__defaultVhost__")}
          ${renderTextField("App", "app", "", "live")}
          ${renderTextField("Stream", "stream", "", "camera01")}
          <div class="checkbox-field">
            <input id="debug-force-close" type="checkbox" name="force" checked />
            <label for="debug-force-close">force=true</label>
          </div>
          <div class="toolbar-actions">
            <button class="danger-button" type="submit">关闭流</button>
          </div>
        </form>
        <form id="debug-snap-form" class="form-grid">
          ${renderTextField("Snapshot URL", "url", "", "rtsp://127.0.0.1/live/camera01")}
          ${renderTextField("Timeout(s)", "timeout_sec", "10", "10", "number")}
          ${renderTextField("Expire(s)", "expire_sec", "30", "30", "number")}
          <div class="toolbar-actions">
            <button class="button" type="submit">抓图</button>
          </div>
        </form>
        ${
          state.ui.debug.snapResult?.data_url
            ? `
              <div class="panel" style="margin-top: 16px;">
                <div class="panel-header">
                  <div>
                    <h3>截图结果</h3>
                    <p>${escapeHtml(state.ui.debug.snapResult.content_type || "image/jpeg")}</p>
                  </div>
                  <div class="section-actions">
                    <button class="ghost-button" data-action="copy" data-value="${escapeAttr(state.ui.debug.snapResult.data_url)}">复制 Data URL</button>
                  </div>
                </div>
                <img class="snap-preview" src="${escapeAttr(state.ui.debug.snapResult.data_url)}" alt="ZLM snapshot preview" />
              </div>
            `
            : ""
        }
      </div>
      <div class="panel">
        <div class="panel-header">
          <div>
            <h3>Hook 时间线</h3>
            <p>查看该节点最近收到的 Hook 事件和去重处理状态。</p>
          </div>
        </div>
        <div class="toolbar-actions">
          <button class="button" data-action="debug-load-hooks">加载 Hook 时间线</button>
        </div>
        ${renderHookTimeline(state.ui.debug.hooksResult)}
      </div>
    </section>
  `;
}

function renderTaskOverview(detail, recordsPage, streams, diffPaths) {
  return `
    <div class="overview-grid">
      <div class="metric">
        <label>当前 Attempt</label>
        <strong>${escapeHtml(detail.current_attempt ? `${detail.current_attempt.attempt_no}` : "0")}</strong>
        <span class="subtle">${escapeHtml(detail.current_attempt?.status || "pending")}</span>
      </div>
      <div class="metric">
        <label>执行节点</label>
        <strong>${escapeHtml(detail.current_attempt?.node_id || detail.task.assigned_node_id || "未分配")}</strong>
        <span class="subtle">${escapeHtml(detail.current_attempt?.worker_kind || detail.task.type)}</span>
      </div>
      <div class="metric">
        <label>录像摘要</label>
        <strong>${escapeHtml(String(recordsPage.total))}</strong>
        <span class="subtle">最近 5 条已加载</span>
      </div>
      <div class="metric">
        <label>流绑定</label>
        <strong>${escapeHtml(String(streams.length))}</strong>
        <span class="subtle">${escapeHtml(streams.map((item) => `${item.app}/${item.stream}`).join(", ") || "暂无")}</span>
      </div>
    </div>
    <div class="split-grid">
      <div class="panel">
        <div class="panel-header">
          <div>
            <h3>最近事件</h3>
            <p>从任务详情自带的 recent_events 展示。</p>
          </div>
        </div>
        <div class="event-list">
          ${
            detail.recent_events.length
              ? detail.recent_events
                  .map(
                    (event) => `
                      <article class="event-item">
                        <div class="toolbar-actions">
                          <span class="tag">${escapeHtml(event.source)}</span>
                          <span class="tag">${escapeHtml(event.event_type)}</span>
                          <span class="subtle">${escapeHtml(formatTime(event.created_at))}</span>
                        </div>
                        <div class="subtle">${escapeHtml(event.event_level)}</div>
                        <pre class="json-block">${escapeHtml(JSON.stringify(event.payload, null, 2))}</pre>
                      </article>
                    `,
                  )
                  .join("")
              : renderInlineEmpty("暂无事件。")
          }
        </div>
      </div>
      <div class="panel">
        <div class="panel-header">
          <div>
            <h3>规格差异</h3>
            <p>requested_spec 与 resolved_spec 的差异路径。</p>
          </div>
        </div>
        <div class="diff-list">
          ${
            diffPaths.length
              ? diffPaths.map((path) => `<div class="diff-item mono">${escapeHtml(path)}</div>`).join("")
              : renderInlineEmpty("requested_spec 与 resolved_spec 当前没有差异路径。")
          }
        </div>
      </div>
    </div>
  `;
}

function renderTaskEventsTab(taskId, eventsPage) {
  const params = state.route.searchParams;
  return `
    <form id="task-events-filter-form" class="filters">
      ${renderTextField("Attempt", "attempt_no", params.get("attempt_no") || "", "留空表示全部")}
      ${renderSelectField("Source", "source", ["", "core", "agent", "ffmpeg", "zlm_api", "zlm_hook", "scheduler", "user"], params.get("source") || "", (value) => value || "全部")}
      ${renderTextField("Event Type", "event_type", params.get("event_type") || "", "task_started")}
      <div class="toolbar-actions">
        <button class="button" type="submit">筛选事件</button>
      </div>
    </form>
    <div class="event-list">
      ${
        eventsPage.items.length
          ? eventsPage.items
              .map(
                (event) => `
                  <article class="event-item">
                    <div class="toolbar-actions">
                      <span class="tag">${escapeHtml(event.source)}</span>
                      <span class="tag">${escapeHtml(event.event_type)}</span>
                      <span class="subtle">${escapeHtml(formatTime(event.created_at))}</span>
                    </div>
                    <div class="subtle">attempt ${escapeHtml(String(event.attempt_no || 0))} · ${escapeHtml(event.event_level)}</div>
                    <pre class="json-block">${escapeHtml(JSON.stringify(event.payload, null, 2))}</pre>
                  </article>
                `,
              )
              .join("")
          : renderInlineEmpty("当前筛选没有事件。")
      }
    </div>
    <div class="pager">${renderPager("task-events", eventsPage, taskId)}</div>
  `;
}

function renderTaskLogsTab(taskId, logs, params) {
  return `
    <form id="task-logs-filter-form" class="filters">
      ${renderTextField("Attempt", "log_attempt_no", params.get("log_attempt_no") || "", "默认当前 attempt")}
      ${renderSelectField("Stream", "log_stream", ["merged", "stdout", "stderr"], params.get("log_stream") || "merged")}
      ${renderTextField("Limit", "log_limit", params.get("log_limit") || "200", "1 - 500")}
      <div class="toolbar-actions">
        <button class="button" type="submit">读取日志</button>
      </div>
    </form>
    <pre class="log-block">${escapeHtml(
      logs.lines.length
        ? logs.lines.map((line) => `${formatTime(line.ts)} [${line.stream}] ${line.line}`).join("\n")
        : "当前 attempt 没有日志。",
    )}</pre>
    ${
      logs.next_cursor
        ? `<div class="pager"><button class="ghost-button" data-action="load-more-logs" data-task-id="${taskId}" data-cursor="${logs.next_cursor}">加载更早日志</button></div>`
        : ""
    }
  `;
}

function renderCreateDrawer() {
  const open = state.ui.createOpen;
  const templates = state.cache.templates || [];
  const draft = state.ui.createDraft;
  return `
    <div class="drawer-backdrop ${open ? "open" : ""}" data-action="close-create-drawer"></div>
    <aside class="drawer ${open ? "open" : ""}">
      <div class="section-header">
        <div>
          <div class="brand-mark">CREATE TASK</div>
          <h3>任务创建向导</h3>
          <p>固定 7 步：类型、模板、输入源、处理与发布、恢复与调度、预览 resolved_spec、提交。</p>
        </div>
        <div class="section-actions">
          <button class="ghost-button" data-action="close-create-drawer">关闭</button>
        </div>
      </div>
      <div class="wizard-steps">
        ${["类型", "模板", "输入源", "处理发布", "恢复调度", "规格预览", "提交"]
          .map(
            (label, index) => `
              <div class="wizard-step ${state.ui.createStep === index + 1 ? "active" : ""}">
                <strong>0${index + 1}</strong>
                <span>${escapeHtml(label)}</span>
              </div>
            `,
          )
          .join("")}
      </div>
      <div class="panel">
        ${renderCreateStep(state.ui.createStep, draft, templates)}
      </div>
      ${
        state.ui.createError
          ? renderErrorPanel("创建向导错误", errorMessage(state.ui.createError))
          : ""
      }
      <div class="modal-actions">
        <button class="ghost-button" data-action="create-prev-step" ${state.ui.createStep === 1 ? "disabled" : ""}>上一步</button>
        ${
          state.ui.createStep < 7
            ? `<button class="button" data-action="create-next-step">${state.ui.createStep === 6 ? "进入提交" : "下一步"}</button>`
            : `<button class="button" data-action="create-submit">提交创建</button>`
        }
      </div>
    </aside>
  `;
}

function renderCreateStep(step, draft, templates) {
  switch (step) {
    case 1:
      return `
        <div class="create-grid">
          ${renderSelectModelField("任务类型", "task_type", TASK_TYPES.map((item) => item.value), draft.task_type, (value) => value)}
          ${renderTextModelField("任务名称", "name", draft.name, "relay-camera-01")}
          ${renderTextModelField("Profile", "profile", draft.profile, "可选")}
          ${renderTextModelField("Tenant", "common.tenant_id", draft.common.tenant_id, "default")}
          ${renderTextModelField("Created By", "common.created_by", draft.common.created_by, "console-user")}
          ${renderTextModelField("Priority", "priority", draft.priority, "0 - 100", "number")}
        </div>
      `;
    case 2: {
      const templateOptions = ["", ...templates.filter((item) => item.type === draft.task_type).map((item) => item.name)];
      return `
        <div class="create-grid">
          ${renderSelectModelField("模板", "template", templateOptions, draft.template || "", (value) => value || "不使用模板")}
          ${renderTextareaModelField("标签", "common.labels_text", draft.common.labels_text, "逗号分隔") }
          ${renderTextModelField("回调地址", "common.callback_url", draft.common.callback_url, "可选")}
        </div>
        <div class="subtle">模板切换会立刻把默认值回填到向导字段，最终提交前仍会通过服务端 <code>resolved_spec</code> 预览再次校验。</div>
      `;
    }
    case 3:
      return renderCreateInputStep(draft);
    case 4:
      return renderCreateProcessStep(draft);
    case 5:
      return renderCreatePolicyStep(draft);
    case 6:
      return `
        <div class="section-header">
          <div>
            <h3>resolved_spec 预览</h3>
            <p>提交前必须先让服务端计算最终结果。</p>
          </div>
          <div class="section-actions">
            <button class="button" data-action="create-preview">生成预览</button>
          </div>
        </div>
        <div class="field-block">
          <label>高级 JSON 覆盖</label>
          <textarea data-model="advanced_json" placeholder='{"publish":{"enable_hls":true}}'>${escapeHtml(draft.advanced_json)}</textarea>
        </div>
        <pre class="json-block">${escapeHtml(JSON.stringify(state.ui.createPreview?.resolved_spec || {}, null, 2) || "{}")}</pre>
      `;
    case 7:
      return `
        <div class="overview-grid">
          ${metricCard("任务类型", draft.task_type)}
          ${metricCard("任务名称", draft.name || "未填写")}
          ${metricCard("启动模式", draft.schedule.start_mode || "immediate")}
          ${metricCard("模板", draft.template || "无")}
        </div>
        <pre class="json-block">${escapeHtml(JSON.stringify(state.ui.createPreview?.resolved_spec || buildDraftPayload(draft), null, 2))}</pre>
        <div class="subtle">提交将调用 <code>POST /api/v1/tasks</code>。如果服务端返回验证错误，会保留当前向导状态。</div>
      `;
    default:
      return "";
  }
}

function renderCreateInputStep(draft) {
  const taskType = draft.task_type;
  const fixedInputKind =
    taskType === "file_transcode" || taskType === "file_to_live"
      ? "file"
      : taskType === "rtp_receive"
        ? "gb_rtp"
        : "";
  const selectableInputKinds =
    taskType === "live_relay"
      ? ["rtsp", "rtmp", "hls", "http_flv", "http_ts"]
      : taskType === "multicast_bridge"
        ? ["rtsp", "rtmp", "hls", "http_flv", "http_ts", "file", "udp_mpegts_multicast", "rtp_multicast"]
        : INPUT_KINDS;
  const inputKind = fixedInputKind || draft.input.kind || "";
  const showUrl = ["rtsp", "rtmp", "hls", "http_flv", "http_ts", "file"].includes(inputKind);
  const showMulticastInput = ["udp_mpegts_multicast", "rtp_multicast"].includes(inputKind);
  const showRtpInput = taskType === "rtp_receive";
  return `
    <div class="create-grid">
      ${
        fixedInputKind
          ? renderStaticModelField("输入类型", fixedInputKind)
          : renderSelectModelField("输入类型", "input.kind", selectableInputKinds, draft.input.kind || "", (value) => value)
      }
      ${showUrl ? renderTextModelField("输入 URL", "input.url", draft.input.url, inputKind === "file" ? "/data/media/input.mp4" : "rtsp://camera/live") : ""}
      ${showMulticastInput ? renderTextModelField("组播地址", "input.group", draft.input.group, "239.0.0.1") : ""}
      ${(showMulticastInput || showRtpInput) ? renderTextModelField("端口", "input.port", draft.input.port, showRtpInput ? "30000" : "5004", "number") : ""}
      ${showMulticastInput ? renderTextModelField("接口 IP", "input.interface_ip", draft.input.interface_ip, "192.168.1.10") : ""}
      ${showMulticastInput ? renderTextModelField("TTL", "input.ttl", draft.input.ttl, "1", "number") : ""}
      ${taskType !== "rtp_receive" ? renderTextModelField("Probe Timeout(ms)", "input.probe_timeout_ms", draft.input.probe_timeout_ms, "7000", "number") : ""}
      ${showRtpInput ? renderTextModelField("TCP Mode", "input.tcp_mode", draft.input.tcp_mode, "0 / 1 / 2", "number") : ""}
      ${showRtpInput ? renderTextModelField("SSRC", "input.ssrc", draft.input.ssrc, "可选", "number") : ""}
      ${(showMulticastInput || showRtpInput) ? renderCheckboxModelField("端口重用", "input.reuse", draft.input.reuse) : ""}
    </div>
  `;
}

function renderCreateProcessStep(draft) {
  const taskType = draft.task_type;
  const publishKind =
    taskType === "file_transcode"
      ? "file"
      : taskType === "file_to_live"
        ? "zlm_ingest"
        : taskType === "rtp_receive" || taskType === "live_relay"
          ? ""
          : draft.publish.kind || "";
  const showProcess = ["file_transcode", "file_to_live", "multicast_bridge"].includes(taskType);
  const showPublishKindSelect = taskType === "multicast_bridge";
  const showPublishUrl = taskType === "file_transcode" || taskType === "file_to_live" || ["file", "zlm_ingest"].includes(publishKind);
  const showPublishNetwork = ["udp_mpegts_multicast", "rtp_multicast"].includes(publishKind);
  const showProtocolFlags = taskType === "live_relay" || taskType === "file_to_live" || taskType === "rtp_receive" || publishKind === "zlm_ingest";
  return `
    <div class="create-grid">
      ${showProcess ? renderTextModelField("处理模式", "process.mode", draft.process.mode, "copy_or_transcode") : ""}
      ${showProcess ? renderTextModelField("Video Codec", "process.video_codec", draft.process.video_codec, "h264") : ""}
      ${showProcess ? renderTextModelField("Audio Codec", "process.audio_codec", draft.process.audio_codec, "aac") : ""}
      ${showProcess ? renderTextModelField("Bitrate", "process.bitrate", draft.process.bitrate, "2000", "number") : ""}
      ${showProcess ? renderTextModelField("FPS", "process.fps", draft.process.fps, "25", "number") : ""}
      ${showProcess ? renderTextModelField("GOP", "process.gop", draft.process.gop, "50", "number") : ""}
      ${showProcess ? renderTextModelField("Profile", "process.profile", draft.process.profile, "baseline") : ""}
      ${showProcess ? renderTextModelField("Preset", "process.preset", draft.process.preset, "veryfast") : ""}
      ${showPublishKindSelect ? renderSelectModelField("发布类型", "publish.kind", ["", ...PUBLISH_KINDS], draft.publish.kind || "", (value) => value || "内部流 / 不显式设置") : renderStaticModelField("发布类型", publishKind || "internal")}
      ${showPublishUrl ? renderTextModelField("发布 URL", "publish.url", draft.publish.url, taskType === "file_transcode" ? "/data/media/output.mp4" : "rtmp://zlm/live/stream") : ""}
      ${showPublishNetwork ? renderTextModelField("发布组播地址", "publish.group", draft.publish.group, "239.1.1.10") : ""}
      ${showPublishNetwork ? renderTextModelField("发布端口", "publish.port", draft.publish.port, "1234", "number") : ""}
      ${showPublishNetwork ? renderTextModelField("发布网卡", "publish.interface_ip", draft.publish.interface_ip, "192.168.1.10") : ""}
      ${showPublishNetwork ? renderTextModelField("发布 TTL", "publish.ttl", draft.publish.ttl, "1", "number") : ""}
      ${showPublishUrl || showPublishNetwork ? renderTextModelField("发布格式", "publish.format", draft.publish.format, "mpegts") : ""}
      ${showProtocolFlags ? renderCheckboxModelField("enable_rtsp", "publish.enable_rtsp", draft.publish.enable_rtsp) : ""}
      ${showProtocolFlags ? renderCheckboxModelField("enable_rtmp", "publish.enable_rtmp", draft.publish.enable_rtmp) : ""}
      ${showProtocolFlags ? renderCheckboxModelField("enable_http_ts", "publish.enable_http_ts", draft.publish.enable_http_ts) : ""}
      ${showProtocolFlags ? renderCheckboxModelField("enable_http_fmp4", "publish.enable_http_fmp4", draft.publish.enable_http_fmp4) : ""}
      ${showProtocolFlags ? renderCheckboxModelField("enable_hls", "publish.enable_hls", draft.publish.enable_hls) : ""}
      ${showProtocolFlags ? renderCheckboxModelField("enable_webrtc", "publish.enable_webrtc", draft.publish.enable_webrtc) : ""}
      ${showProtocolFlags ? renderCheckboxModelField("无人观看自动停止", "publish.stop_on_no_reader", draft.publish.stop_on_no_reader) : ""}
    </div>
  `;
}

function renderCreatePolicyStep(draft) {
  const showRecordFields = Boolean(draft.record.enabled);
  const startMode = draft.schedule.start_mode || "immediate";
  return `
    <div class="create-grid">
      ${renderCheckboxModelField("启用录制", "record.enabled", draft.record.enabled)}
      ${showRecordFields ? renderSelectModelField("录制格式", "record.format", ["", ...RECORD_FORMATS], draft.record.format || "", (value) => value || "默认") : ""}
      ${showRecordFields ? renderTextModelField("录制切片秒数", "record.segment_sec", draft.record.segment_sec, "60", "number") : ""}
      ${showRecordFields ? renderTextModelField("录制路径", "record.save_path", draft.record.save_path, "/data/zlm/record") : ""}
      ${showRecordFields ? renderCheckboxModelField("as_player", "record.as_player", draft.record.as_player) : ""}
      ${renderSelectModelField("恢复策略", "recovery.policy", ["", ...RECOVERY_POLICIES], draft.recovery.policy || "", (value) => value || "默认")}
      ${renderTextModelField("恢复模式", "recovery.resume_mode", draft.recovery.resume_mode, "auto")}
      ${renderCheckboxModelField("孤儿接管", "recovery.orphan_adopt", draft.recovery.orphan_adopt)}
      ${renderTextModelField("最大连续失败", "recovery.max_consecutive_failures", draft.recovery.max_consecutive_failures, "3", "number")}
      ${renderSelectModelField("启动模式", "schedule.start_mode", START_MODES, draft.schedule.start_mode, (value) => value)}
      ${startMode === "at" ? renderTextModelField("Start At", "schedule.start_at", draft.schedule.start_at, "2026-03-30T12:00:00Z") : ""}
      ${startMode === "cron" ? renderTextModelField("Cron", "schedule.cron", draft.schedule.cron, "0 */5 * * * *") : ""}
      ${renderTextareaModelField("required_labels", "resource.required_labels_text", draft.resource.required_labels_text, "逗号分隔")}
      ${renderTextareaModelField("preferred_labels", "resource.preferred_labels_text", draft.resource.preferred_labels_text, "逗号分隔")}
      ${renderTextModelField("network_interface", "resource.network_interface", draft.resource.network_interface, "eth0")}
      ${renderCheckboxModelField("need_gpu", "resource.need_gpu", draft.resource.need_gpu)}
      ${renderTextModelField("slot_class", "resource.slot_class", draft.resource.slot_class, "standard")}
      ${renderTextModelField("max_cpu_percent", "resource.max_cpu_percent", draft.resource.max_cpu_percent, "80", "number")}
    </div>
  `;
}

function renderAuthModal() {
  const open = state.ui.authModalOpen;
  return `
    <div class="modal-backdrop ${open ? "open" : ""}" data-action="close-auth-modal"></div>
    <section class="modal ${open ? "open" : ""}">
      <div class="section-header">
        <div>
          <div class="brand-mark">AUTH</div>
          <h3>Bearer Token</h3>
          <p>当前前端不内建登录流程。启用鉴权时，直接粘贴 JWT Bearer Token。</p>
        </div>
        <div class="section-actions">
          <button class="ghost-button" data-action="close-auth-modal">关闭</button>
        </div>
      </div>
      <div class="field-block">
        <label for="auth-token-input">Authorization Token</label>
        <textarea id="auth-token-input" data-action="auth-token-input" placeholder="eyJhbGciOi..." rows="8">${escapeHtml(state.ui.authDraftToken || state.token)}</textarea>
      </div>
      <div class="modal-actions">
        <button class="ghost-button" data-action="clear-auth-token">清空</button>
        <button class="button" data-action="save-auth-token">保存并刷新</button>
      </div>
    </section>
  `;
}

function renderToasts() {
  return `
    <div class="toast-stack">
      ${state.toasts
        .map(
          (item) => `
            <article class="toast ${item.kind}">
              <strong>${escapeHtml(item.title)}</strong>
              <div class="subtle">${escapeHtml(item.message)}</div>
            </article>
          `,
        )
        .join("")}
    </div>
  `;
}

async function handleClick(event) {
  const link = event.target.closest("[data-link]");
  if (link) {
    event.preventDefault();
    const href = link.getAttribute("href");
    if (href) {
      await navigate(href);
    }
    return;
  }

  const actionTarget = event.target.closest("[data-action]");
  if (!actionTarget) {
    return;
  }
  const action = actionTarget.dataset.action;
  try {
    switch (action) {
      case "refresh-page":
        await refreshSession(true);
        await refreshRoute();
        break;
      case "open-auth-modal":
        state.ui.authDraftToken = state.token;
        state.ui.authModalOpen = true;
        renderApp();
        break;
      case "close-auth-modal":
        state.ui.authModalOpen = false;
        renderApp();
        break;
      case "save-auth-token":
        state.token = (state.ui.authDraftToken || "").trim();
        if (state.token) {
          window.localStorage.setItem(TOKEN_STORAGE_KEY, state.token);
        } else {
          window.localStorage.removeItem(TOKEN_STORAGE_KEY);
        }
        state.ui.authModalOpen = false;
        await refreshSession(false);
        await refreshRoute();
        break;
      case "clear-auth-token":
        state.ui.authDraftToken = "";
        state.token = "";
        window.localStorage.removeItem(TOKEN_STORAGE_KEY);
        await refreshSession(true);
        renderApp();
        break;
      case "open-create-drawer":
        if (!canAccess("task_write")) {
          return;
        }
        await ensureTemplatesLoaded();
        state.ui.createOpen = true;
        state.ui.createError = null;
        renderApp();
        break;
      case "close-create-drawer":
        state.ui.createOpen = false;
        renderApp();
        break;
      case "create-prev-step":
        state.ui.createStep = Math.max(1, state.ui.createStep - 1);
        renderApp();
        break;
      case "create-next-step":
        if (state.ui.createStep === 6 && !state.ui.createPreview) {
          await requestTaskPreview();
        }
        if (state.ui.createStep < 7) {
          state.ui.createStep += 1;
        }
        renderApp();
        break;
      case "create-preview":
        await requestTaskPreview();
        renderApp();
        break;
      case "create-submit":
        await submitTaskCreate();
        break;
      case "reset-task-filters":
        await navigate("/tasks");
        break;
      case "reset-stream-filters":
        await navigate("/streams");
        break;
      case "reset-record-filters":
        await navigate("/records");
        break;
      case "load-more-logs":
        await updateTaskDetailQuery({
          tab: "logs",
          log_cursor: actionTarget.dataset.cursor,
        });
        break;
      case "task-start":
        await performTaskAction(actionTarget.dataset.taskId, "start");
        break;
      case "task-stop":
        await performTaskAction(actionTarget.dataset.taskId, "stop");
        break;
      case "task-cancel":
        await performTaskAction(actionTarget.dataset.taskId, "cancel");
        break;
      case "task-retry":
        await performTaskAction(actionTarget.dataset.taskId, "retry");
        break;
      case "task-clone":
        await cloneTask(actionTarget.dataset.taskId);
        break;
      case "copy":
        await copyText(actionTarget.dataset.value || "");
        break;
      case "close-stream":
        await closeStream(actionTarget.dataset);
        break;
      case "toggle-node-detail":
        await toggleNodeInsight(actionTarget.dataset.nodeId);
        break;
      case "debug-load-sessions":
        await loadDebugSessions();
        break;
      case "debug-load-players":
        await loadDebugPlayers();
        break;
      case "debug-load-statistic":
        await loadDebugStatistics();
        break;
      case "debug-load-hooks":
        await loadDebugHooks();
        break;
      default:
        break;
    }
  } catch (error) {
    console.error(error);
    toast(errorMessage(error), "error");
  }
}

async function handleSubmit(event) {
  const form = event.target;
  if (!(form instanceof HTMLFormElement)) {
    return;
  }
  event.preventDefault();
  try {
    switch (form.id) {
      case "tasks-filter-form":
        await navigate(`/tasks?${buildQueryString(new FormData(form))}`);
        break;
      case "streams-filter-form":
        await navigate(`/streams?${buildQueryString(new FormData(form))}`);
        break;
      case "records-filter-form":
        await navigate(`/records?${buildQueryString(new FormData(form))}`);
        break;
      case "task-events-filter-form":
        await updateTaskDetailQuery({
          tab: "events",
          page: "1",
          attempt_no: formValue(form, "attempt_no"),
          source: formValue(form, "source"),
          event_type: formValue(form, "event_type"),
        });
        break;
      case "task-logs-filter-form":
        await updateTaskDetailQuery({
          tab: "logs",
          log_attempt_no: formValue(form, "log_attempt_no"),
          log_stream: formValue(form, "log_stream"),
          log_limit: formValue(form, "log_limit"),
          log_cursor: "",
        });
        break;
      case "debug-media-form":
        await loadDebugMedia(new FormData(form));
        break;
      case "debug-kick-form":
        await kickDebugSession(new FormData(form));
        break;
      case "debug-kick-batch-form":
        await submitDebugKickBatch(new FormData(form));
        break;
      case "debug-close-form":
        await submitDebugClose(new FormData(form));
        break;
      case "debug-snap-form":
        await submitDebugSnap(new FormData(form));
        break;
      default:
        break;
    }
  } catch (error) {
    console.error(error);
    toast(errorMessage(error), "error");
  }
}

async function handleChange(event) {
  const target = event.target;
  if (!(target instanceof HTMLElement)) {
    return;
  }
  try {
    if (target.dataset.model) {
      updateCreateDraftFromElement(target);
      if (target.dataset.model === "template" && target instanceof HTMLSelectElement) {
        await applySelectedTemplate(target.value);
      }
    }
    if (target.id === "debug-node-id" && target instanceof HTMLSelectElement) {
      state.ui.debug.nodeId = target.value;
      state.ui.debug.mediaResult = null;
      state.ui.debug.sessionsResult = null;
      state.ui.debug.playersResult = null;
      state.ui.debug.statisticResult = null;
      state.ui.debug.threadsLoadResult = null;
      state.ui.debug.workThreadsLoadResult = null;
      state.ui.debug.snapResult = null;
      state.ui.debug.hooksResult = null;
      renderApp();
    }
  } catch (error) {
    console.error(error);
    toast(errorMessage(error), "error");
  }
}

function handleInput(event) {
  const target = event.target;
  if (!(target instanceof HTMLElement)) {
    return;
  }
  if (target.dataset.model) {
    updateCreateDraftFromElement(target);
  }
  if (target.dataset.action === "auth-token-input" && target instanceof HTMLTextAreaElement) {
    state.ui.authDraftToken = target.value;
  }
}

function updateCreateDraftFromElement(target) {
  const path = target.dataset.model;
  if (!path) {
    return;
  }
  const value =
    target instanceof HTMLInputElement && target.type === "checkbox"
      ? target.checked
      : target.value;
  setPath(state.ui.createDraft, path, value);
  if (path === "task_type") {
    normalizeDraftForTaskType(state.ui.createDraft, value);
    state.ui.createDraft.template = "";
  }
  state.ui.createPreview = null;
  state.ui.createError = null;
}

async function navigate(href) {
  window.history.pushState({}, "", href);
  state.route = parseRoute(window.location.pathname, window.location.search);
  await refreshRoute();
}

function parseRoute(pathname, search) {
  const cleanPath = pathname || "/tasks";
  const searchParams = new URLSearchParams(search || "");
  const taskMatch = cleanPath.match(/^\/tasks\/([^/]+)$/);
  if (taskMatch) {
    return { name: "task-detail", path: cleanPath, searchParams, params: { id: taskMatch[1] } };
  }
  if (cleanPath === "/streams") return { name: "streams", path: cleanPath, searchParams, params: {} };
  if (cleanPath === "/multicast") return { name: "multicast", path: cleanPath, searchParams, params: {} };
  if (cleanPath === "/records") return { name: "records", path: cleanPath, searchParams, params: {} };
  if (cleanPath === "/nodes") return { name: "nodes", path: cleanPath, searchParams, params: {} };
  if (cleanPath.startsWith("/debug")) return { name: "debug", path: cleanPath, searchParams, params: {} };
  return { name: "tasks", path: "/tasks", searchParams, params: {} };
}

function currentRouteTitle() {
  switch (state.route.name) {
    case "task-detail":
      return "任务详情";
    case "streams":
      return "流中心";
    case "multicast":
      return "组播中心";
    case "records":
      return "录像中心";
    case "nodes":
      return "节点中心";
    case "debug":
      return "调试台";
    default:
      return "任务中心";
  }
}

function currentRouteSubtitle() {
  switch (state.route.name) {
    case "task-detail":
      return "基本信息、事件、日志、requested_spec、resolved_spec。";
    case "streams":
      return "内部流、播放地址、viewer 状态和管理员操作。";
    case "multicast":
      return "multicast_bridge 任务、组地址、TTL、上下游。";
    case "records":
      return "录像文件检索和任务回溯。";
    case "nodes":
      return "节点健康、能力矩阵、当前任务和 ZLM 概览。";
    case "debug":
      return "原始 ZLM 调试接口的安全入口。";
    default:
      return "任务列表、筛选、创建、重试、停止和克隆。";
  }
}

function canAccess(permission) {
  if (!state.session) {
    return !state.sessionError;
  }
  return state.session.permissions.includes(permission);
}

function sessionSubtitle() {
  if (!state.session) {
    return state.sessionError ? errorMessage(state.sessionError) : "正在建立会话";
  }
  const tenant = state.session.tenant_id ? `tenant ${state.session.tenant_id}` : "platform scope";
  return `${state.session.role} · ${tenant}`;
}

async function apiRequest(path, options = {}) {
  const headers = new Headers(options.headers || {});
  if (state.token) {
    headers.set("Authorization", `Bearer ${state.token}`);
  }
  let body = options.body;
  if (body && typeof body === "object" && !(body instanceof FormData) && !(body instanceof Blob)) {
    headers.set("Content-Type", "application/json");
    body = JSON.stringify(body);
  }

  const response = await fetch(path, {
    method: options.method || "GET",
    headers,
    body,
  });
  const contentType = response.headers.get("content-type") || "";
  const payload =
    response.status === 204
      ? null
      : contentType.includes("application/json")
        ? await response.json()
        : await response.text();
  if (!response.ok) {
    const error = new Error(
      payload?.message || `HTTP ${response.status}`,
    );
    error.status = response.status;
    error.payload = payload;
    throw error;
  }
  return payload;
}

async function fetchTaskDetail(taskId, force) {
  if (!force && state.cache.taskDetails.has(taskId)) {
    return state.cache.taskDetails.get(taskId);
  }
  const detail = await apiRequest(`/api/v1/tasks/${taskId}`);
  state.cache.taskDetails.set(taskId, detail);
  return detail;
}

async function fetchNodesCached(force) {
  if (!force && state.cache.nodes) {
    return state.cache.nodes;
  }
  const nodes = await apiRequest("/api/v1/nodes");
  state.cache.nodes = nodes;
  return nodes;
}

async function fetchTemplatesCached(force) {
  if (!force && state.cache.templates) {
    return state.cache.templates;
  }
  const templates = await apiRequest("/api/v1/templates");
  state.cache.templates = templates;
  return templates;
}

async function fetchTemplateDetail(templateId, force) {
  if (!force && state.cache.templateDetails.has(templateId)) {
    return state.cache.templateDetails.get(templateId);
  }
  const detail = await apiRequest(`/api/v1/templates/${templateId}`);
  state.cache.templateDetails.set(templateId, detail);
  return detail;
}

async function ensureTemplatesLoaded() {
  if (!canAccess("template_read")) {
    state.cache.templates = [];
    return [];
  }
  return await fetchTemplatesCached(true);
}

async function applySelectedTemplate(templateName) {
  if (!templateName) {
    return;
  }
  const summary = (state.cache.templates || []).find((item) => item.name === templateName);
  if (!summary) {
    return;
  }
  const detail = await fetchTemplateDetail(summary.id, false);
  applyTaskSpecDefaultsToDraft(state.ui.createDraft, detail.default_spec || {});
  if (detail.profile) {
    state.ui.createDraft.profile = detail.profile;
  }
  state.ui.createPreview = null;
  state.ui.createError = null;
  renderApp();
}

async function performTaskAction(taskId, action) {
  if (!taskId) {
    return;
  }
  const confirmed = window.confirm(`确认执行 ${action} ?`);
  if (!confirmed) {
    return;
  }
  await apiRequest(`/api/v1/tasks/${taskId}/${action}`, {
    method: "POST",
  });
  toast(`任务 ${shortId(taskId)} 已执行 ${action}`, "success");
  state.cache.taskDetails.delete(taskId);
  await refreshRoute();
}

async function cloneTask(taskId) {
  const name = window.prompt("输入克隆后的任务名称", "task-copy");
  if (!name) {
    return;
  }
  const cloned = await apiRequest(`/api/v1/tasks/${taskId}/clone`, {
    method: "POST",
    body: { name },
  });
  toast(`已克隆任务 ${shortId(cloned.id)}`, "success");
  await navigate(`/tasks/${cloned.id}`);
}

async function closeStream(data) {
  const confirmed = window.confirm(`确认关闭流 ${data.app}/${data.stream} ?`);
  if (!confirmed) {
    return;
  }
  await apiRequest("/api/v1/debug/zlm/close-stream", {
    method: "POST",
    body: {
      node_id: data.nodeId,
      schema: data.schema,
      vhost: data.vhost,
      app: data.app,
      stream: data.stream,
      force: true,
    },
  });
  toast(`已请求关闭 ${data.app}/${data.stream}`, "success");
  await refreshRoute();
}

async function toggleNodeInsight(nodeId) {
  if (state.ui.openNodeId === nodeId) {
    state.ui.openNodeId = "";
    renderApp();
    return;
  }
  state.ui.openNodeId = nodeId;
  renderApp();
  const insight = await loadNodeInsight(nodeId);
  state.cache.nodeInsights.set(nodeId, insight);
  renderApp();
}

async function loadNodeInsight(nodeId) {
  const [tasksPage, heartbeats, media, sessions, players, statistic, threadsLoad, workThreadsLoad] = await Promise.all([
    apiRequest(`/api/v1/tasks?assigned_node_id=${encodeURIComponent(nodeId)}&page_size=6&sort_by=updated_at&sort_order=desc`),
    apiRequest(`/api/v1/nodes/${encodeURIComponent(nodeId)}/heartbeats?limit=12`).catch(() => []),
    canAccess("debug_read") ? apiRequest(`/api/v1/debug/zlm/media?node_id=${encodeURIComponent(nodeId)}`).catch(() => null) : Promise.resolve(null),
    canAccess("debug_read") ? apiRequest(`/api/v1/debug/zlm/sessions?node_id=${encodeURIComponent(nodeId)}`).catch(() => null) : Promise.resolve(null),
    canAccess("debug_read") ? apiRequest(`/api/v1/debug/zlm/players?node_id=${encodeURIComponent(nodeId)}`).catch(() => null) : Promise.resolve(null),
    canAccess("debug_read") ? apiRequest(`/api/v1/debug/zlm/statistic?node_id=${encodeURIComponent(nodeId)}`).catch(() => null) : Promise.resolve(null),
    canAccess("debug_read") ? apiRequest(`/api/v1/debug/zlm/threads-load?node_id=${encodeURIComponent(nodeId)}`).catch(() => null) : Promise.resolve(null),
    canAccess("debug_read") ? apiRequest(`/api/v1/debug/zlm/work-threads-load?node_id=${encodeURIComponent(nodeId)}`).catch(() => null) : Promise.resolve(null),
  ]);
  return { tasksPage, heartbeats, media, sessions, players, statistic, threadsLoad, workThreadsLoad };
}

async function loadDebugMedia(formData) {
  ensureDebugNode();
  const query = new URLSearchParams({ node_id: state.ui.debug.nodeId });
  ["schema", "vhost", "app", "stream"].forEach((key) => {
    const value = (formData.get(key) || "").toString().trim();
    if (value) {
      query.set(key, value);
    }
  });
  state.ui.debug.mediaResult = await apiRequest(`/api/v1/debug/zlm/media?${query.toString()}`);
  state.ui.debug.lastError = null;
  renderApp();
}

async function loadDebugSessions() {
  ensureDebugNode();
  state.ui.debug.sessionsResult = await apiRequest(`/api/v1/debug/zlm/sessions?node_id=${encodeURIComponent(state.ui.debug.nodeId)}`);
  state.ui.debug.lastError = null;
  renderApp();
}

async function loadDebugPlayers() {
  ensureDebugNode();
  state.ui.debug.playersResult = await apiRequest(`/api/v1/debug/zlm/players?node_id=${encodeURIComponent(state.ui.debug.nodeId)}`);
  state.ui.debug.lastError = null;
  renderApp();
}

async function loadDebugStatistics() {
  ensureDebugNode();
  const [statistic, threadsLoad, workThreadsLoad] = await Promise.all([
    apiRequest(`/api/v1/debug/zlm/statistic?node_id=${encodeURIComponent(state.ui.debug.nodeId)}`),
    apiRequest(`/api/v1/debug/zlm/threads-load?node_id=${encodeURIComponent(state.ui.debug.nodeId)}`),
    apiRequest(`/api/v1/debug/zlm/work-threads-load?node_id=${encodeURIComponent(state.ui.debug.nodeId)}`),
  ]);
  state.ui.debug.statisticResult = statistic;
  state.ui.debug.threadsLoadResult = threadsLoad;
  state.ui.debug.workThreadsLoadResult = workThreadsLoad;
  state.ui.debug.lastError = null;
  renderApp();
}

async function loadDebugHooks() {
  ensureDebugNode();
  state.ui.debug.hooksResult = await apiRequest(`/api/v1/debug/hooks?node_id=${encodeURIComponent(state.ui.debug.nodeId)}&limit=40`);
  state.ui.debug.lastError = null;
  renderApp();
}

async function kickDebugSession(formData) {
  ensureDebugNode();
  const sessionId = (formData.get("session_id") || "").toString().trim();
  if (!sessionId) {
    toast("Session ID 不能为空", "error");
    return;
  }
  await apiRequest("/api/v1/debug/zlm/kick-session", {
    method: "POST",
    body: {
      node_id: state.ui.debug.nodeId,
      session_id: sessionId,
    },
  });
  toast(`已请求踢出 Session ${sessionId}`, "success");
}

async function submitDebugKickBatch(formData) {
  ensureDebugNode();
  await apiRequest("/api/v1/debug/zlm/kick-sessions", {
    method: "POST",
    body: {
      node_id: state.ui.debug.nodeId,
      local_port: toNullableNumber(formData.get("local_port")),
      peer_ip: (formData.get("peer_ip") || "").toString().trim() || null,
    },
  });
  toast("已发送批量踢会话请求", "success");
}

async function submitDebugClose(formData) {
  ensureDebugNode();
  await apiRequest("/api/v1/debug/zlm/close-stream", {
    method: "POST",
    body: {
      node_id: state.ui.debug.nodeId,
      schema: (formData.get("schema") || "").toString().trim(),
      vhost: (formData.get("vhost") || "").toString().trim(),
      app: (formData.get("app") || "").toString().trim(),
      stream: (formData.get("stream") || "").toString().trim(),
      force: Boolean(formData.get("force")),
    },
  });
  toast("已发送关流请求", "success");
}

async function submitDebugSnap(formData) {
  ensureDebugNode();
  const query = new URLSearchParams({
    node_id: state.ui.debug.nodeId,
    url: (formData.get("url") || "").toString().trim(),
    timeout_sec: String(toNullableNumber(formData.get("timeout_sec")) || 10),
    expire_sec: String(toNullableNumber(formData.get("expire_sec")) || 30),
  });
  state.ui.debug.snapResult = await apiRequest(`/api/v1/debug/zlm/snap?${query.toString()}`);
  state.ui.debug.lastError = null;
  renderApp();
}

function ensureDebugNode() {
  if (!state.ui.debug.nodeId) {
    throw new Error("请先选择调试节点");
  }
}

async function requestTaskPreview() {
  try {
    const payload = buildDraftPayload(state.ui.createDraft);
    state.ui.createPreview = await apiRequest("/api/v1/tasks/preview", {
      method: "POST",
      body: payload,
    });
    state.ui.createError = null;
    toast("resolved_spec 预览已更新", "success");
  } catch (error) {
    state.ui.createError = error;
    toast(errorMessage(error), "error");
  }
}

async function submitTaskCreate() {
  try {
    const payload = buildDraftPayload(state.ui.createDraft);
    const task = await apiRequest("/api/v1/tasks", {
      method: "POST",
      headers: {
        "Idempotency-Key": window.crypto?.randomUUID?.() || `console-${Date.now()}`,
      },
      body: payload,
    });
    toast(`任务 ${task.name} 已创建`, "success");
    state.ui.createOpen = false;
    state.ui.createStep = 1;
    state.ui.createDraft = createDefaultDraft();
    state.ui.createPreview = null;
    await navigate(`/tasks/${task.id}`);
  } catch (error) {
    state.ui.createError = error;
    toast(errorMessage(error), "error");
    renderApp();
  }
}

function buildDraftPayload(draft) {
  const payload = {
    type: draft.task_type,
    name: draft.name.trim(),
    priority: toNumberOrDefault(draft.priority, 50),
    common: {},
    input: {},
    process: {},
    publish: {},
    record: {},
    recovery: {},
    schedule: {},
    resource: {},
  };

  setIfPresent(payload, "template", draft.template);
  setIfPresent(payload, "profile", draft.profile);

  setIfPresent(payload.common, "tenant_id", draft.common.tenant_id);
  setIfPresent(payload.common, "created_by", draft.common.created_by);
  setIfPresent(payload.common, "callback_url", draft.common.callback_url);
  setIfList(payload.common, "labels", draft.common.labels_text);

  setIfPresent(payload.input, "kind", draft.input.kind);
  setIfPresent(payload.input, "url", draft.input.url);
  setIfPresent(payload.input, "group", draft.input.group);
  setIfNumber(payload.input, "port", draft.input.port);
  setIfPresent(payload.input, "interface_ip", draft.input.interface_ip);
  setIfNumber(payload.input, "ttl", draft.input.ttl);
  setIfBoolean(payload.input, "reuse", draft.input.reuse);
  setIfNumber(payload.input, "probe_timeout_ms", draft.input.probe_timeout_ms);
  setIfNumber(payload.input, "tcp_mode", draft.input.tcp_mode);
  setIfNumber(payload.input, "ssrc", draft.input.ssrc);

  setIfPresent(payload.process, "mode", draft.process.mode);
  setIfPresent(payload.process, "video_codec", draft.process.video_codec);
  setIfPresent(payload.process, "audio_codec", draft.process.audio_codec);
  setIfNumber(payload.process, "bitrate", draft.process.bitrate);
  setIfNumber(payload.process, "fps", draft.process.fps);
  setIfNumber(payload.process, "gop", draft.process.gop);
  setIfPresent(payload.process, "profile", draft.process.profile);
  setIfPresent(payload.process, "preset", draft.process.preset);

  setIfPresent(payload.publish, "kind", draft.publish.kind);
  setIfPresent(payload.publish, "url", draft.publish.url);
  setIfPresent(payload.publish, "group", draft.publish.group);
  setIfNumber(payload.publish, "port", draft.publish.port);
  setIfPresent(payload.publish, "interface_ip", draft.publish.interface_ip);
  setIfNumber(payload.publish, "ttl", draft.publish.ttl);
  setIfPresent(payload.publish, "format", draft.publish.format);
  setIfBoolean(payload.publish, "enable_rtsp", draft.publish.enable_rtsp);
  setIfBoolean(payload.publish, "enable_rtmp", draft.publish.enable_rtmp);
  setIfBoolean(payload.publish, "enable_http_ts", draft.publish.enable_http_ts);
  setIfBoolean(payload.publish, "enable_http_fmp4", draft.publish.enable_http_fmp4);
  setIfBoolean(payload.publish, "enable_hls", draft.publish.enable_hls);
  setIfBoolean(payload.publish, "enable_webrtc", draft.publish.enable_webrtc);
  setIfBoolean(payload.publish, "stop_on_no_reader", draft.publish.stop_on_no_reader);

  setIfBoolean(payload.record, "enabled", draft.record.enabled);
  setIfPresent(payload.record, "format", draft.record.format);
  setIfNumber(payload.record, "segment_sec", draft.record.segment_sec);
  setIfPresent(payload.record, "save_path", draft.record.save_path);
  setIfBoolean(payload.record, "as_player", draft.record.as_player);

  setIfPresent(payload.recovery, "policy", draft.recovery.policy);
  setIfPresent(payload.recovery, "resume_mode", draft.recovery.resume_mode);
  setIfBoolean(payload.recovery, "orphan_adopt", draft.recovery.orphan_adopt);
  setIfNumber(payload.recovery, "max_consecutive_failures", draft.recovery.max_consecutive_failures);

  setIfPresent(payload.schedule, "start_mode", draft.schedule.start_mode);
  setIfPresent(payload.schedule, "start_at", draft.schedule.start_at);
  setIfPresent(payload.schedule, "cron", draft.schedule.cron);

  setIfList(payload.resource, "required_labels", draft.resource.required_labels_text);
  setIfList(payload.resource, "preferred_labels", draft.resource.preferred_labels_text);
  setIfPresent(payload.resource, "network_interface", draft.resource.network_interface);
  setIfBoolean(payload.resource, "need_gpu", draft.resource.need_gpu);
  setIfPresent(payload.resource, "slot_class", draft.resource.slot_class);
  setIfNumber(payload.resource, "max_cpu_percent", draft.resource.max_cpu_percent);

  pruneEmptyObjects(payload);

  const advanced = parseAdvancedJson(draft.advanced_json);
  return deepMerge(payload, advanced);
}

function createDefaultDraft() {
  const draft = {
    task_type: "live_relay",
    template: "",
    profile: "",
    name: "",
    priority: "50",
    advanced_json: "{}",
    common: {
      tenant_id: "default",
      created_by: "console",
      callback_url: "",
      labels_text: "",
    },
    input: {
      kind: "rtsp",
      url: "",
      group: "",
      port: "",
      interface_ip: "",
      ttl: "",
      reuse: false,
      probe_timeout_ms: "",
      tcp_mode: "",
      ssrc: "",
    },
    process: {
      mode: "",
      video_codec: "",
      audio_codec: "",
      bitrate: "",
      fps: "",
      gop: "",
      profile: "",
      preset: "",
    },
    publish: {
      kind: "",
      url: "",
      group: "",
      port: "",
      interface_ip: "",
      ttl: "",
      format: "",
      enable_rtsp: true,
      enable_rtmp: true,
      enable_http_ts: true,
      enable_http_fmp4: true,
      enable_hls: false,
      enable_webrtc: false,
      stop_on_no_reader: false,
    },
    record: {
      enabled: false,
      format: "",
      segment_sec: "",
      save_path: "",
      as_player: false,
    },
    recovery: {
      policy: "",
      resume_mode: "",
      orphan_adopt: true,
      max_consecutive_failures: "",
    },
    schedule: {
      start_mode: "immediate",
      start_at: "",
      cron: "",
    },
    resource: {
      required_labels_text: "",
      preferred_labels_text: "",
      network_interface: "",
      need_gpu: false,
      slot_class: "",
      max_cpu_percent: "",
    },
  };
  normalizeDraftForTaskType(draft, draft.task_type);
  return draft;
}

function normalizeDraftForTaskType(draft, taskType) {
  draft.task_type = taskType;
  switch (taskType) {
    case "file_transcode":
      draft.input.kind = "file";
      draft.publish.kind = "file";
      break;
    case "file_to_live":
      draft.input.kind = "file";
      draft.publish.kind = "zlm_ingest";
      break;
    case "multicast_bridge":
      draft.input.kind = "udp_mpegts_multicast";
      draft.publish.kind = "zlm_ingest";
      break;
    case "rtp_receive":
      draft.input.kind = "gb_rtp";
      draft.publish.kind = "";
      break;
    default:
      draft.input.kind = draft.input.kind || "rtsp";
      draft.publish.kind = draft.publish.kind || "";
      break;
  }
}

function renderNodeMetric(node) {
  return `
    <div class="metric-panel">
      <label>${escapeHtml(node.node_name)}</label>
      <strong>${node.healthy ? "ONLINE" : "OFFLINE"}</strong>
      <div class="subtle">${escapeHtml(node.hostname)} · ${escapeHtml(node.network_mode)}</div>
      <div class="inline-list" style="margin-top: 12px;">
        ${node.zlm_version ? `<span class="tag">${escapeHtml(node.zlm_version)}</span>` : ""}
        <span class="tag">CPU ${formatPercent(node.cpu_percent)}</span>
        <span class="tag">MEM ${formatPercent(node.mem_percent)}</span>
        <span class="tag">RUN ${escapeHtml(String(node.running_tasks ?? 0))}</span>
      </div>
    </div>
  `;
}

function renderExpandedNodeInsight(node, insight) {
  return `
    <div class="overview-grid">
      ${metricCard("CPU", formatPercent(node.cpu_percent))}
      ${metricCard("MEM", formatPercent(node.mem_percent))}
      ${metricCard("Disk", formatPercent(node.disk_percent))}
      ${metricCard("ZLM", node.zlm_alive === false ? "down" : "up")}
      ${metricCard("FFmpeg", node.ffmpeg_alive === false ? "down" : "up")}
      ${metricCard("最近心跳", formatTime(node.last_seen_at))}
    </div>
    <div class="split-grid" style="margin-top: 16px;">
      <div class="panel">
        <div class="panel-header">
          <div>
            <h3>能力矩阵</h3>
            <p>${escapeHtml(node.labels.join(", ") || "无 labels")}</p>
          </div>
        </div>
        <div class="inline-list">
          ${node.ffmpeg_protocols.slice(0, 8).map((item) => `<span class="tag">${escapeHtml(item)}</span>`).join("")}
        </div>
        <div class="subtle" style="margin-top: 12px;">Encoders: ${escapeHtml(node.ffmpeg_encoders.slice(0, 6).join(", ") || "—")}</div>
        <div class="subtle">Interfaces: ${escapeHtml(node.interfaces.join(", ") || "—")}</div>
      </div>
      <div class="panel">
        <div class="panel-header">
          <div>
            <h3>ZLM 概览</h3>
            <p>媒体、Session、玩家数量，以及线程负载和对象统计。</p>
          </div>
        </div>
        <div class="overview-grid">
          ${metricCard("Media", safeCollectionSize(insight?.media?.data))}
          ${metricCard("Sessions", safeCollectionSize(insight?.sessions?.data))}
          ${metricCard("Players", safeCollectionSize(insight?.players?.data))}
          ${metricCard("Threads Avg", formatThreadLoadAverage(insight?.threadsLoad))}
          ${metricCard("Work Avg", formatThreadLoadAverage(insight?.workThreadsLoad))}
          ${metricCard("Objects", formatStatisticObjectCount(insight?.statistic))}
        </div>
      </div>
    </div>
    <div class="split-grid" style="margin-top: 16px;">
      <div class="panel">
        <div class="panel-header">
          <div>
            <h3>最近心跳</h3>
            <p>最近 12 次 heartbeat 的负载快照。</p>
          </div>
        </div>
        ${renderHeartbeatTimeline(insight?.heartbeats)}
      </div>
      <div class="panel">
        <div class="panel-header">
          <div>
            <h3>线程与对象统计</h3>
            <p>getThreadsLoad / getWorkThreadsLoad / getStatistic 汇总。</p>
          </div>
        </div>
        ${renderThreadLoadPanel(insight?.threadsLoad, insight?.workThreadsLoad)}
        <pre class="json-block">${escapeHtml(JSON.stringify(insight?.statistic || {}, null, 2))}</pre>
      </div>
    </div>
    <div class="panel" style="margin-top: 16px;">
      <div class="panel-header">
        <div>
          <h3>当前任务</h3>
          <p>按节点过滤的最近任务。</p>
        </div>
      </div>
      <div class="event-list">
        ${
          insight?.tasksPage?.items?.length
            ? insight.tasksPage.items
                .map(
                  (task) => `
                    <article class="event-item">
                      <div class="toolbar-actions">
                        <a href="/tasks/${task.id}" data-link class="mono">${shortId(task.id)}</a>
                        ${statusPill(task.status)}
                      </div>
                      <div><strong>${escapeHtml(task.name)}</strong></div>
                      <div class="subtle">${escapeHtml(task.type)} · priority ${escapeHtml(String(task.priority))}</div>
                    </article>
                  `,
                )
                .join("")
            : renderInlineEmpty("当前没有关联任务。")
        }
      </div>
    </div>
  `;
}

function renderTaskActions(task, compact) {
  if (!canAccess("task_write")) {
    return `<a class="ghost-button" href="/tasks/${task.id}" data-link>查看</a>`;
  }
  const actions = [];
  if (["CREATED", "FAILED", "CANCELED", "VALIDATING", "QUEUED"].includes(task.status)) {
    actions.push(`<button class="ghost-button" data-action="task-start" data-task-id="${task.id}">启动</button>`);
  }
  if (["DISPATCHING", "STARTING", "RUNNING", "RECOVERING"].includes(task.status)) {
    actions.push(`<button class="soft-button" data-action="task-stop" data-task-id="${task.id}">停止</button>`);
    actions.push(`<button class="danger-button" data-action="task-cancel" data-task-id="${task.id}">取消</button>`);
  }
  if (["FAILED", "LOST"].includes(task.status)) {
    actions.push(`<button class="ghost-button" data-action="task-retry" data-task-id="${task.id}">重试</button>`);
  }
  actions.push(`<button class="ghost-button" data-action="task-clone" data-task-id="${task.id}">克隆</button>`);
  if (!compact) {
    actions.push(`<a class="ghost-button" href="/tasks/${task.id}" data-link>详情</a>`);
  }
  return `<div class="toolbar-actions">${actions.join("")}</div>`;
}

function renderTaskDetailTab(taskId, activeTab, tab, label) {
  const query = new URLSearchParams(state.route.searchParams.toString());
  query.set("tab", tab);
  return `<a href="/tasks/${taskId}?${query.toString()}" data-link class="tab ${activeTab === tab ? "active" : ""}">${escapeHtml(label)}</a>`;
}

function renderRolePill(role) {
  return `<span class="role-pill">${escapeHtml(role)}</span>`;
}

function renderPlayUrls(stream, node, task) {
  const urls = Array.isArray(stream) ? stream : [];
  return urls.length
    ? `<div class="play-url-list">${urls.map((url) => `<button class="play-url" data-action="copy" data-value="${escapeAttr(url)}">${escapeHtml(url)}</button>`).join("")}</div>`
    : "—";
}

function renderRecordingLabel(task) {
  const enabled = Boolean(task?.resolved_spec?.record?.enabled);
  const format = task?.resolved_spec?.record?.format;
  return enabled ? `enabled${format ? ` (${format})` : ""}` : "disabled";
}

function renderDebugResult(value) {
  if (!value) {
    return renderInlineEmpty("还没有查询结果。");
  }
  return `<pre class="json-block">${escapeHtml(JSON.stringify(value, null, 2))}</pre>`;
}

function renderThreadLoadPanel(threadsLoad, workThreadsLoad) {
  if (!threadsLoad && !workThreadsLoad) {
    return renderInlineEmpty("还没有线程负载结果。");
  }
  return `
    <div class="overview-grid">
      ${metricCard("Threads Avg", formatThreadLoadAverage(threadsLoad))}
      ${metricCard("Threads Max", formatThreadLoadMax(threadsLoad))}
      ${metricCard("Work Avg", formatThreadLoadAverage(workThreadsLoad))}
      ${metricCard("Work Max", formatThreadLoadMax(workThreadsLoad))}
    </div>
    <pre class="json-block">${escapeHtml(JSON.stringify({
      threads: threadsLoad?.data || threadsLoad || [],
      work_threads: workThreadsLoad?.data || workThreadsLoad || [],
    }, null, 2))}</pre>
  `;
}

function renderHeartbeatTimeline(heartbeats) {
  if (!Array.isArray(heartbeats) || !heartbeats.length) {
    return renderInlineEmpty("当前没有心跳历史。");
  }
  return `
    <div class="event-list">
      ${heartbeats
        .map(
          (item) => `
            <article class="event-item">
              <div class="toolbar-actions">
                <span class="subtle">${escapeHtml(formatTime(item.received_at || item.node_time))}</span>
                <span class="tag">${item.zlm_alive === false ? "zlm_down" : "zlm_up"}</span>
                <span class="tag">${item.ffmpeg_alive === false ? "ffmpeg_down" : "ffmpeg_up"}</span>
              </div>
              <div class="inline-list">
                <span class="tag">CPU ${formatPercent(item.cpu_percent)}</span>
                <span class="tag">MEM ${formatPercent(item.mem_percent)}</span>
                <span class="tag">DISK ${formatPercent(item.disk_percent)}</span>
                <span class="tag">RUN ${escapeHtml(String(item.running_tasks ?? 0))}</span>
                <span class="tag">SLOT ${formatPercent((item.slot_usage ?? 0) * 100)}</span>
              </div>
            </article>
          `,
        )
        .join("")}
    </div>
  `;
}

function renderHookTimeline(items) {
  if (!Array.isArray(items) || !items.length) {
    return renderInlineEmpty("当前没有 Hook 事件。");
  }
  return `
    <div class="event-list">
      ${items
        .map(
          (item) => `
            <article class="event-item">
              <div class="toolbar-actions">
                <span class="tag">${escapeHtml(item.hook_name)}</span>
                <span class="subtle">${escapeHtml(formatTime(item.received_at))}</span>
              </div>
              <div class="subtle">${escapeHtml(item.processed_at ? `processed @ ${formatTime(item.processed_at)}` : "pending")}</div>
              <pre class="json-block">${escapeHtml(JSON.stringify(item.payload, null, 2))}</pre>
            </article>
          `,
        )
        .join("")}
    </div>
  `;
}

function renderPager(kind, page, taskId) {
  const totalPages = Math.max(1, Math.ceil(page.total / page.page_size));
  const prevPage = Math.max(1, page.page - 1);
  const nextPage = Math.min(totalPages, page.page + 1);
  const prevDisabled = page.page <= 1;
  const nextDisabled = page.page >= totalPages;
  const prevHref = pageHref(kind, prevPage, taskId);
  const nextHref = pageHref(kind, nextPage, taskId);
  return `
    <span class="subtle">第 ${page.page} / ${totalPages} 页</span>
    <a class="ghost-button ${prevDisabled ? "disabled" : ""}" href="${prevHref}" data-link ${prevDisabled ? "aria-disabled=true" : ""}>上一页</a>
    <a class="ghost-button ${nextDisabled ? "disabled" : ""}" href="${nextHref}" data-link ${nextDisabled ? "aria-disabled=true" : ""}>下一页</a>
  `;
}

function pageHref(kind, pageNumber, taskId) {
  const query = new URLSearchParams(state.route.searchParams.toString());
  switch (kind) {
    case "records":
      query.set("page", String(pageNumber));
      return `/records?${query.toString()}`;
    case "task-events":
      query.set("tab", "events");
      query.set("page", String(pageNumber));
      return `/tasks/${taskId}?${query.toString()}`;
    default:
      query.set("page", String(pageNumber));
      return `/tasks?${query.toString()}`;
  }
}

function renderTextField(label, name, value, placeholder, type = "text") {
  return `
    <label class="field">
      <span>${escapeHtml(label)}</span>
      <input type="${type}" name="${escapeAttr(name)}" value="${escapeAttr(value)}" placeholder="${escapeAttr(placeholder)}" />
    </label>
  `;
}

function renderDateTimeField(label, name, value) {
  return renderTextField(label, name, value, "2026-03-29T00:00:00Z");
}

function renderSelectField(label, name, values, selected, labelForValue = (value) => value || "全部", idMode = false) {
  const id = idMode ? name : "";
  return `
    <label class="field">
      <span>${escapeHtml(label)}</span>
      <select ${id ? `id="${escapeAttr(id)}"` : ""} name="${escapeAttr(name)}">
        ${values
          .map((value) => `<option value="${escapeAttr(value)}" ${value === selected ? "selected" : ""}>${escapeHtml(labelForValue(value))}</option>`)
          .join("")}
      </select>
    </label>
  `;
}

function renderTextModelField(label, path, value, placeholder, type = "text") {
  return `
    <label class="field">
      <span>${escapeHtml(label)}</span>
      <input type="${type}" data-model="${escapeAttr(path)}" value="${escapeAttr(value || "")}" placeholder="${escapeAttr(placeholder)}" />
    </label>
  `;
}

function renderTextareaModelField(label, path, value, placeholder) {
  return `
    <label class="field-block">
      <span>${escapeHtml(label)}</span>
      <textarea data-model="${escapeAttr(path)}" placeholder="${escapeAttr(placeholder)}">${escapeHtml(value || "")}</textarea>
    </label>
  `;
}

function renderSelectModelField(label, path, values, selected, labelForValue = (value) => value || "未设置") {
  return `
    <label class="field">
      <span>${escapeHtml(label)}</span>
      <select data-model="${escapeAttr(path)}">
        ${values
          .map((value) => `<option value="${escapeAttr(value)}" ${value === selected ? "selected" : ""}>${escapeHtml(labelForValue(value))}</option>`)
          .join("")}
      </select>
    </label>
  `;
}

function renderCheckboxModelField(label, path, checked) {
  return `
    <label class="checkbox-field">
      <input type="checkbox" data-model="${escapeAttr(path)}" ${checked ? "checked" : ""} />
      <span>${escapeHtml(label)}</span>
    </label>
  `;
}

function renderStaticModelField(label, value) {
  return `
    <div class="metric">
      <label>${escapeHtml(label)}</label>
      <strong>${escapeHtml(String(value || "—"))}</strong>
    </div>
  `;
}

function metricCard(label, value, rawValue = false) {
  return `
    <div class="metric">
      <label>${escapeHtml(label)}</label>
      <strong>${rawValue ? value : escapeHtml(String(value))}</strong>
    </div>
  `;
}

function statusPill(status) {
  return `<span class="status-pill ${STATUS_THEME[status] || "status-created"}">${escapeHtml(status)}</span>`;
}

function renderLoadingPanel() {
  return `
    <section class="empty-state">
      <h3>正在加载</h3>
      <p>控制面正在同步任务、节点、流与调试数据。</p>
    </section>
  `;
}

function renderErrorPanel(title, message) {
  return `
    <section class="auth-panel">
      <h3>${escapeHtml(title)}</h3>
      <p>${escapeHtml(message)}</p>
      <div class="actions">
        <button class="ghost-button" data-action="refresh-page">重试</button>
      </div>
    </section>
  `;
}

function renderAuthRequired() {
  return `
    <section class="auth-panel">
      <h3>需要认证</h3>
      <p>${escapeHtml(errorMessage(state.sessionError) || "当前环境启用了鉴权，请先提供 Bearer Token。")}</p>
      <div class="actions">
        <button class="button" data-action="open-auth-modal">输入 Token</button>
      </div>
    </section>
  `;
}

function renderEmptyState(title, message) {
  return `
    <section class="empty-state">
      <h3>${escapeHtml(title)}</h3>
      <p>${escapeHtml(message)}</p>
    </section>
  `;
}

function renderInlineEmpty(message) {
  return `<div class="subtle">${escapeHtml(message)}</div>`;
}

function renderFatal(error) {
  return `
    <div class="boot-shell">
      <div class="boot-panel">
        <div class="boot-mark">FATAL</div>
        <h1>前端初始化失败</h1>
        <p>${escapeHtml(error?.message || String(error))}</p>
      </div>
    </div>
  `;
}

function nodeLabel(node) {
  if (!node) {
    return "—";
  }
  return `${node.node_name}`;
}

function viewerCountLabel(viewerCount, hasViewer) {
  if (viewerCount !== null && viewerCount !== undefined) return String(viewerCount);
  if (hasViewer === true) return ">=1";
  if (hasViewer === false) return "0";
  return "—";
}

function multicastRowModel(task, spec, detail, node, streams) {
  const input = spec.input || {};
  const publish = spec.publish || {};
  const usePublish = publish.group || publish.port || publish.interface_ip;
  const progress = deriveLatestProgress(detail?.recent_events || []);
  const streamRuntime = Array.isArray(streams) ? streams.find((item) => item.bitrate_kbps || item.viewer_count !== undefined) : null;
  return {
    mode: `${input.kind || "unknown"} -> ${publish.kind || "internal"}`,
    group: usePublish ? publish.group || "—" : input.group || "—",
    port: String(usePublish ? publish.port || "—" : input.port || "—"),
    interfaceIp: usePublish ? publish.interface_ip || "—" : input.interface_ip || "—",
    ttl: String(usePublish ? publish.ttl || "—" : input.ttl || "—"),
    node: nodeLabel(node),
    bitrate: formatBitrateKbps(streamRuntime?.bitrate_kbps ?? progress?.bitrate_kbps),
    lastError: deriveLastIssue(detail?.recent_events || []) || "—",
    binding: streamRuntime?.play_urls?.[0] || publish.url || `${publish.group || "—"}:${publish.port || "—"}`,
  };
}

function deriveLastIssue(events) {
  const list = Array.isArray(events) ? events : [];
  const critical = list.find((event) => ["error", "warn"].includes(String(event.event_level).toLowerCase()));
  if (!critical) {
    return "";
  }
  return (
    critical.payload?.failure_reason ||
    critical.payload?.message ||
    critical.event_type ||
    "最近存在异常事件"
  );
}

function computeDiffPaths(left, right, prefix = "") {
  const paths = [];
  const leftObj = left && typeof left === "object" ? left : {};
  const rightObj = right && typeof right === "object" ? right : {};
  const keys = new Set([...Object.keys(leftObj), ...Object.keys(rightObj)]);
  for (const key of keys) {
    const nextPrefix = prefix ? `${prefix}.${key}` : key;
    const leftValue = leftObj[key];
    const rightValue = rightObj[key];
    if (isPlainObject(leftValue) && isPlainObject(rightValue)) {
      paths.push(...computeDiffPaths(leftValue, rightValue, nextPrefix));
      continue;
    }
    if (JSON.stringify(leftValue) !== JSON.stringify(rightValue)) {
      paths.push(nextPrefix);
    }
  }
  return paths;
}

function parseAdvancedJson(text) {
  const raw = String(text || "").trim();
  if (!raw || raw === "{}") {
    return {};
  }
  try {
    const parsed = JSON.parse(raw);
    return isPlainObject(parsed) ? parsed : {};
  } catch (_error) {
    return {};
  }
}

function setIfPresent(target, key, value) {
  const trimmed = String(value ?? "").trim();
  if (trimmed) {
    target[key] = trimmed;
  }
}

function setIfNumber(target, key, value) {
  const parsed = toOptionalNumber(value);
  if (parsed !== undefined) {
    target[key] = parsed;
  }
}

function setIfBoolean(target, key, value) {
  if (typeof value === "boolean") {
    target[key] = value;
  }
}

function setIfList(target, key, text) {
  const items = String(text || "")
    .split(",")
    .map((value) => value.trim())
    .filter(Boolean);
  if (items.length) {
    target[key] = items;
  }
}

function pruneEmptyObjects(target) {
  Object.keys(target).forEach((key) => {
    const value = target[key];
    if (Array.isArray(value)) {
      return;
    }
    if (isPlainObject(value)) {
      pruneEmptyObjects(value);
      if (Object.keys(value).length === 0) {
        delete target[key];
      }
    }
  });
}

function deepMerge(base, overlay) {
  const output = structuredClone(base);
  mergeInto(output, overlay);
  return output;
}

function mergeInto(target, overlay) {
  if (!isPlainObject(overlay)) {
    return;
  }
  for (const [key, value] of Object.entries(overlay)) {
    if (isPlainObject(value) && isPlainObject(target[key])) {
      mergeInto(target[key], value);
    } else {
      target[key] = value;
    }
  }
}

function isPlainObject(value) {
  return value && typeof value === "object" && !Array.isArray(value);
}

function copyIfPresent(from, to, keys) {
  keys.forEach((key) => {
    const value = from.get(key);
    if (value) {
      to.set(key, value);
    }
  });
}

function buildQueryString(formData) {
  const query = new URLSearchParams();
  for (const [key, value] of formData.entries()) {
    const stringValue = String(value).trim();
    if (stringValue) {
      query.set(key, stringValue);
    }
  }
  return query.toString();
}

async function updateTaskDetailQuery(updates) {
  const query = new URLSearchParams(state.route.searchParams.toString());
  Object.entries(updates).forEach(([key, value]) => {
    if (value === undefined || value === null || value === "") {
      query.delete(key);
    } else {
      query.set(key, value);
    }
  });
  await navigate(`/tasks/${state.route.params.id}?${query.toString()}`);
}

function formValue(form, name) {
  return (new FormData(form).get(name) || "").toString().trim();
}

function shortId(value) {
  return String(value || "").slice(0, 8);
}

function formatTime(value) {
  if (!value) {
    return "—";
  }
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) {
    return String(value);
  }
  return date.toLocaleString("zh-CN", {
    hour12: false,
  });
}

function formatBytes(bytes) {
  if (bytes === null || bytes === undefined) {
    return "—";
  }
  const numeric = Number(bytes);
  if (!Number.isFinite(numeric)) {
    return String(bytes);
  }
  const units = ["B", "KB", "MB", "GB", "TB"];
  let value = numeric;
  let index = 0;
  while (value >= 1024 && index < units.length - 1) {
    value /= 1024;
    index += 1;
  }
  return `${value.toFixed(value >= 10 || index === 0 ? 0 : 1)} ${units[index]}`;
}

function formatPercent(value) {
  if (value === null || value === undefined) {
    return "—";
  }
  const numeric = Number(value);
  return Number.isFinite(numeric) ? `${numeric.toFixed(1)}%` : "—";
}

function formatBitrateKbps(value) {
  if (value === null || value === undefined) {
    return "未上报";
  }
  const numeric = Number(value);
  return Number.isFinite(numeric) ? `${numeric.toFixed(numeric >= 100 ? 0 : 1)} kbps` : "未上报";
}

function formatThreadLoadAverage(payload) {
  const items = Array.isArray(payload?.data) ? payload.data : Array.isArray(payload) ? payload : [];
  if (!items.length) {
    return "—";
  }
  const total = items.reduce((sum, item) => sum + (Number(item.load) || 0), 0);
  return `${(total / items.length).toFixed(1)}%`;
}

function formatThreadLoadMax(payload) {
  const items = Array.isArray(payload?.data) ? payload.data : Array.isArray(payload) ? payload : [];
  if (!items.length) {
    return "—";
  }
  const max = items.reduce((value, item) => Math.max(value, Number(item.load) || 0), 0);
  return `${max.toFixed(1)}%`;
}

function formatStatisticObjectCount(payload) {
  const stats = payload?.data && typeof payload.data === "object" ? payload.data : payload;
  if (!stats || typeof stats !== "object") {
    return "—";
  }
  const total = Object.values(stats).reduce((sum, value) => sum + (Number(value) || 0), 0);
  return String(total);
}

function safeCollectionSize(value) {
  return Array.isArray(value) ? String(value.length) : "—";
}

function toOptionalNumber(value) {
  const raw = String(value ?? "").trim();
  if (!raw) {
    return undefined;
  }
  const numeric = Number(raw);
  return Number.isFinite(numeric) ? numeric : undefined;
}

function toNullableNumber(value) {
  const numeric = toOptionalNumber(value);
  return numeric === undefined ? null : numeric;
}

function toNumberOrDefault(value, fallback) {
  const numeric = toOptionalNumber(value);
  return numeric === undefined ? fallback : numeric;
}

function errorMessage(error) {
  if (!error) {
    return "未知错误";
  }
  return error.payload?.message || error.message || String(error);
}

function isAuthError(error) {
  return Number(error?.status) === 403;
}

function shouldRenderAuthRequired(error) {
  return isAuthError(error) && !state.session;
}

function setPath(target, path, value) {
  const parts = path.split(".");
  let current = target;
  for (let index = 0; index < parts.length - 1; index += 1) {
    const part = parts[index];
    if (!isPlainObject(current[part])) {
      current[part] = {};
    }
    current = current[part];
  }
  current[parts.at(-1)] = value;
}

function deriveLatestProgress(events) {
  const list = Array.isArray(events) ? events : [];
  const progressEvent = list.find((event) => event.event_type === "task_progress");
  return progressEvent?.payload || null;
}

function applyTaskSpecDefaultsToDraft(draft, spec) {
  if (!isPlainObject(spec)) {
    return;
  }
  if (spec.type) {
    normalizeDraftForTaskType(draft, String(spec.type));
  }
  if (spec.name !== undefined) draft.name = String(spec.name || "");
  if (spec.profile !== undefined) draft.profile = String(spec.profile || "");
  if (spec.priority !== undefined) draft.priority = String(spec.priority ?? "");
  applyDraftSectionDefaults(draft.common, spec.common, {
    tenant_id: "string",
    created_by: "string",
    callback_url: "string",
    labels: "list:labels_text",
  });
  applyDraftSectionDefaults(draft.input, spec.input, {
    kind: "string",
    url: "string",
    group: "string",
    port: "number",
    interface_ip: "string",
    ttl: "number",
    reuse: "boolean",
    probe_timeout_ms: "number",
    tcp_mode: "number",
    ssrc: "number",
  });
  applyDraftSectionDefaults(draft.process, spec.process, {
    mode: "string",
    video_codec: "string",
    audio_codec: "string",
    bitrate: "number",
    fps: "number",
    gop: "number",
    profile: "string",
    preset: "string",
  });
  applyDraftSectionDefaults(draft.publish, spec.publish, {
    kind: "string",
    url: "string",
    group: "string",
    port: "number",
    interface_ip: "string",
    ttl: "number",
    format: "string",
    enable_rtsp: "boolean",
    enable_rtmp: "boolean",
    enable_http_ts: "boolean",
    enable_http_fmp4: "boolean",
    enable_hls: "boolean",
    enable_webrtc: "boolean",
    stop_on_no_reader: "boolean",
  });
  applyDraftSectionDefaults(draft.record, spec.record, {
    enabled: "boolean",
    format: "string",
    segment_sec: "number",
    save_path: "string",
    as_player: "boolean",
  });
  applyDraftSectionDefaults(draft.recovery, spec.recovery, {
    policy: "string",
    resume_mode: "string",
    orphan_adopt: "boolean",
    max_consecutive_failures: "number",
  });
  applyDraftSectionDefaults(draft.schedule, spec.schedule, {
    start_mode: "string",
    start_at: "string",
    cron: "string",
  });
  applyDraftSectionDefaults(draft.resource, spec.resource, {
    required_labels: "list:required_labels_text",
    preferred_labels: "list:preferred_labels_text",
    network_interface: "string",
    need_gpu: "boolean",
    slot_class: "string",
    max_cpu_percent: "number",
  });
}

function applyDraftSectionDefaults(target, source, mapping) {
  if (!isPlainObject(source)) {
    return;
  }
  Object.entries(mapping).forEach(([key, kind]) => {
    if (!(key in source)) {
      return;
    }
    const value = source[key];
    if (kind === "string") {
      target[key] = value === null || value === undefined ? "" : String(value);
      return;
    }
    if (kind === "number") {
      target[key] = value === null || value === undefined ? "" : String(value);
      return;
    }
    if (kind === "boolean") {
      target[key] = Boolean(value);
      return;
    }
    if (kind.startsWith("list:")) {
      const field = kind.split(":")[1];
      target[field] = Array.isArray(value) ? value.join(", ") : "";
    }
  });
}

async function copyText(value) {
  try {
    await navigator.clipboard.writeText(value);
    toast("已复制到剪贴板", "success");
  } catch (_error) {
    toast("复制失败", "error");
  }
}

function toast(message, kind = "success") {
  const title = kind === "error" ? "操作失败" : "操作成功";
  const item = {
    id: `${Date.now()}-${Math.random()}`,
    title,
    message,
    kind,
  };
  state.toasts = [...state.toasts, item].slice(-4);
  renderApp();
  window.setTimeout(() => {
    state.toasts = state.toasts.filter((toastItem) => toastItem.id !== item.id);
    renderApp();
  }, 2600);
}

function escapeHtml(value) {
  return String(value ?? "")
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");
}

function escapeAttr(value) {
  return escapeHtml(value).replaceAll("\n", " ");
}
