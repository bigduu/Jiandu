"use strict";
const $ = (id) => document.getElementById(id);
const state = { scope: "global", project: "", session: "", query: "", status: "", cursor: "", page: 1 };
const controllers = new Map();
const labels = { global: "Global", project: "Project", session: "Session" };
const statuses = { active: "有效", stale: "待更新", superseded: "已替代", contradicted: "有冲突", archived: "已归档" };
const types = { user: "用户", feedback: "反馈", project: "项目", reference: "参考" };
let viewGeneration = 0, detailGeneration = 0, catalogGeneration = 0, nextCursor = "", sessionState = null;
const dateFormat = new Intl.DateTimeFormat("zh-CN", { dateStyle: "medium", timeStyle: "short" });
function node(tag, className, text) {
  const element = document.createElement(tag);
  if (className) element.className = className;
  if (text !== undefined && text !== null) element.textContent = String(text);
  return element;
}
function text(id, value) { $(id).textContent = value; }
function date(value) { const parsed = new Date(value); return value ? (Number.isNaN(parsed.getTime()) ? value : dateFormat.format(parsed)) : "未记录"; }
function abort(kind) { controllers.get(kind)?.abort(); controllers.delete(kind); }
function abortView() { ["list", "status", "detail"].forEach(abort); detailGeneration += 1; }
async function request(kind, path, parameters = {}) {
  abort(kind);
  const controller = new AbortController();
  controllers.set(kind, controller);
  const url = new URL(path, window.location.origin);
  for (const [key, value] of Object.entries(parameters)) if (value !== "" && value !== undefined) url.searchParams.set(key, value);
  try {
    const response = await fetch(url, { method: "GET", headers: { "X-Jiandu-Console": "1" }, cache: "no-store", signal: controller.signal });
    const raw = await response.text();
    let data;
    try { data = JSON.parse(raw); } catch { if (response.ok) throw new Error("本地服务返回了无法读取的数据。"); }
    if (!response.ok) throw new Error(data?.error || raw || `请求失败（${response.status}）。`);
    return data;
  } finally { if (controllers.get(kind) === controller) controllers.delete(kind); }
}
function placeholder(parent, title, description, kind = "empty", retry) {
  const box = node("div", `placeholder ${kind}`);
  const mark = node("span", kind === "loading" ? "spinner" : "placeholder-mark", kind === "loading" ? "" : "◇");
  mark.setAttribute("aria-hidden", "true");
  box.append(mark, node("h3", "", title), node("p", "", description));
  if (retry) { const button = node("button", "", "重新载入"); button.type = "button"; button.addEventListener("click", retry); box.append(button); }
  parent.replaceChildren(box);
}
function context() {
  if (state.scope === "session") return state.session ? { session_id: state.session } : null;
  if (state.scope === "project") return state.project ? { scope: "project", project_id: state.project } : null;
  return { scope: "global" };
}
function updateNavigation() {
  const session = state.scope === "session";
  document.querySelectorAll("[data-scope]").forEach((button) => {
    const active = button.dataset.scope === state.scope;
    button.classList.toggle("active", active); button.setAttribute("aria-pressed", String(active));
  });
  $("project-picker").hidden = state.scope !== "project"; $("session-picker").hidden = !session;
  $("status-filter").hidden = session; $("health").hidden = session;
  text("scope-name", labels[state.scope].toUpperCase());
  text("page-title", { global: "全局记忆", project: "项目记忆", session: "会话笔记" }[state.scope]);
  const description = { global: "跨项目共享的事实、偏好与参考。", project: "同一个 Project 下，共享一份持久知识。", session: "当前会话保留的笔记，不推断所属 Project。" }[state.scope];
  text("page-description", description); text("scope-guide", description);
  text("context-id", session ? state.session || "选择 Session" : state.scope === "project" ? state.project || "选择 Project" : "Global");
  text("list-heading", session ? "笔记主题" : "记忆列表");
  $("query").placeholder = session ? "搜索主题名称或笔记内容…" : "搜索关键词、标题或内容…";
  text("query-hint", session ? "按主题与笔记原文搜索；空笔记也可以打开查看。" : "留空搜索可浏览全部原始记忆；关键词搜索使用本地词法索引。");
}
function clearView(loading = true) {
  viewGeneration += 1; abortView(); nextCursor = ""; sessionState = null;
  $("first-page").disabled = true; $("next-page").disabled = true;
  text("result-count", loading ? "载入中…" : "等待选择"); text("page-number", `第 ${state.page} 页`);
  $("list").setAttribute("aria-busy", String(loading));
  $("detail").setAttribute("aria-busy", "false");
  placeholder($("detail"), "打开一条记忆", "从列表选择条目，在这里阅读原文与来源。");
  text("index-state", loading ? "载入中…" : "等待选择"); text("index-time", "—"); text("memory-total", "—");
  text("dream-state", loading ? "载入中…" : "等待选择"); text("dream-time", "仅用于方向提示");
  $("status-note").hidden = true; text("status-note", "");
  $("dream-panel").hidden = true; $("dream-panel").open = false; text("dream-content", "");
  $("session-info").hidden = true; text("session-info", "");
  return viewGeneration;
}
function loadView(reset = true) {
  if (reset) { state.cursor = ""; state.page = 1; }
  updateNavigation();
  const parameters = context();
  const generation = clearView(Boolean(parameters));
  if (!parameters) {
    const session = state.scope === "session";
    placeholder($("list"), session ? "暂无 Session" : "暂无 Project", "目录中还没有可浏览的记录。新增记录后，点击「刷新目录」。");
    placeholder($("detail"), session ? "等待会话笔记" : "等待项目记忆", "此处显示所选条目的原始内容。");
    return;
  }
  placeholder($("list"), "正在读取本地内容", "正在载入当前范围的记录…", "loading");
  if (state.scope === "session") placeholder($("detail"), "打开一条笔记", "选择一个主题，阅读该会话的完整笔记。");
  loadList(parameters, generation);
  if (state.scope !== "session") loadStatus(parameters, generation);
}
async function loadList(parameters, generation) {
  const session = state.scope === "session";
  try {
    const data = await request("list", session ? "/api/session" : "/api/memories", { ...parameters, q: state.query, status: session ? "" : state.status, cursor: state.cursor });
    if (generation !== viewGeneration) return;
    const items = session ? data.topics : data.items;
    sessionState = session ? data.state : null;
    nextCursor = data.next_cursor || "";
    text("result-count", `${items.length} / ${data.matched_count} 条`);
    $("first-page").disabled = !state.cursor; $("next-page").disabled = !nextCursor;
    if (session) {
      $("session-info").hidden = false;
      text("session-info", `Session · 创建 ${date(data.state?.created_at)} · 更新 ${date(data.state?.updated_at)}`);
    }
    if (!items.length) {
      const filtered = Boolean(state.query || (!session && state.status));
      placeholder($("list"), filtered ? "没有匹配的内容" : session ? "暂无笔记主题" : "这里还没有记忆", filtered ? "试试更短的关键词，或清空搜索与状态筛选。" : "新记录写入后，刷新目录即可查看。");
      return;
    }
    $("list").replaceChildren();
    items.forEach((item) => {
      const card = node("button", "memory-card"); card.type = "button"; card.setAttribute("aria-pressed", "false");
      card.append(node("span", "card-title", session ? item.topic : item.title), node("span", "card-summary", item.summary || (session ? "空笔记 · 可打开查看" : "暂无摘要")));
      const chips = node("span", "chips");
      if (session) chips.append(node("span", "chip", "会话笔记"));
      else { chips.append(statusChip(item.status), node("span", "chip", types[item.type] || item.type)); (item.tags || []).slice(0, 3).forEach((tag) => chips.append(node("span", "chip", `#${tag}`))); }
      card.append(chips); card.addEventListener("click", () => openDetail(item, card, parameters, generation, session)); $("list").append(card);
    });
  } catch (error) {
    if (generation !== viewGeneration || error.name === "AbortError") return;
    text("result-count", "载入失败");
    $("first-page").disabled = !state.cursor;
    placeholder($("list"), "暂时无法读取内容", `${error.message} 请检查本地 Jiandu 服务，或重试。`, "error", () => loadView(false));
  } finally { if (generation === viewGeneration) $("list").setAttribute("aria-busy", "false"); }
}
async function loadStatus(parameters, generation) {
  try {
    const data = await request("status", "/api/status", parameters);
    if (generation !== viewGeneration) return;
    const index = data.index || {}, dream = data.dream, snapshot = dream?.snapshot;
    text("index-state", { ready: "已就绪", empty: "暂无条目", missing: "索引缺失", invalid: "索引无效" }[index.state] || "状态未知");
    text("index-time", index.generated_at || data.inspection?.last_reindex_at ? `更新 ${date(index.generated_at || data.inspection.last_reindex_at)}` : "未记录生成时间");
    text("memory-total", data.inspection?.total_memories ?? "—");
    text("dream-state", data.dream_error ? "暂不可用" : snapshot ? dream.stale ? "快照待更新" : "可用于参考" : "暂无快照");
    text("dream-time", snapshot ? `生成 ${date(snapshot.generated_at)}` : "仅用于方向提示");
    $("dream-panel").hidden = !snapshot;
    if (snapshot) text("dream-content", snapshot.content || "");
    const notes = [];
    if (["missing", "invalid"].includes(index.state)) notes.push("可先清空搜索，浏览原始记忆。需要重建时，请由获授权的 memory 工具执行 rebuild；此控制台只读。");
    if (index.error) notes.push(`索引：${index.error}`);
    if (data.dream_error) notes.push(`Dream 状态读取失败：${data.dream_error} 记忆浏览仍可使用。`);
    if (snapshot && dream.stale) notes.push("Dream 已过期，仅供方向参考，请核对当前记忆原文。");
    text("status-note", notes.join(" ")); $("status-note").hidden = !notes.length;
  } catch (error) {
    if (generation !== viewGeneration || error.name === "AbortError") return;
    text("index-state", "状态不可用"); text("dream-state", "状态不可用");
    text("status-note", `状态读取失败：${error.message} 仍可浏览记忆；刷新目录可重试状态读取。`); $("status-note").hidden = false;
  }
}
function statusChip(status) { return node("span", `chip ${statuses[status] ? status : ""}`, statuses[status] || status || "未记录状态"); }
function facts(entries) {
  const list = node("dl", "facts");
  entries.forEach(([label, value]) => { const row = node("div"); row.append(node("dt", "", label), node("dd", "", value || "未记录")); list.append(row); });
  return list;
}
function actor(value) { return typeof value === "string" ? value : [value?.kind, value?.id, value?.actor].filter(Boolean).join(" · "); }
async function openDetail(item, card, parameters, generation, session) {
  if (generation !== viewGeneration) return;
  const token = ++detailGeneration;
  document.querySelectorAll(".memory-card").forEach((button) => { button.classList.toggle("selected", button === card); button.setAttribute("aria-pressed", String(button === card)); });
  placeholder($("detail"), "正在读取原文", "正在载入所选条目的完整内容…", "loading");
  $("detail").setAttribute("aria-busy", "true");
  try {
    const data = await request("detail", session ? "/api/topic" : "/api/memory", { ...parameters, ...(session ? { topic: item.topic } : { id: item.id }) });
    if (generation !== viewGeneration || token !== detailGeneration) return;
    const detail = $("detail"); detail.replaceChildren();
    if (session) {
      detail.append(node("h2", "", data.topic), node("code", "detail-id", parameters.session_id), facts([["范围", "Session · 会话笔记"], ["主题", data.topic], ["会话创建", date(sessionState?.created_at)], ["会话更新", date(sessionState?.updated_at)]]), node("h3", "", "笔记原文"), node("pre", "plain-content", data.content || "这条笔记暂时没有内容。"));
    } else {
      const memory = data.frontmatter;
      detail.append(node("h2", "", memory.title), node("code", "detail-id", memory.id));
      const chips = node("div", "chips"); chips.append(statusChip(memory.status), node("span", "chip", types[memory.type] || memory.type));
      (memory.tags || []).forEach((tag) => chips.append(node("span", "chip", `#${tag}`))); detail.append(chips);
      detail.append(facts([["范围", memory.scope === "project" ? `Project · ${parameters.project_id}` : "Global"], ["类型", types[memory.type] || memory.type], ["创建时间", date(memory.created_at)], ["更新时间", date(memory.updated_at)], ["创建者", actor(memory.created_by)], ["更新者", actor(memory.updated_by)]]));
      detail.append(node("h3", "", "来源"));
      const sources = node("ul", "source-list");
      if (!memory.sources?.length) sources.append(node("li", "", "未记录来源"));
      else memory.sources.forEach((source) => sources.append(node("li", "", [source.kind, source.id, source.message_range?.join(" – ")].filter(Boolean).join(" · "))));
      detail.append(sources);
      const relations = Object.entries(memory.relations || {}).filter(([, ids]) => ids?.length);
      if (relations.length) {
        detail.append(node("h3", "", "关联记录")); const links = node("ul", "source-list");
        relations.forEach(([kind, ids]) => links.append(node("li", "", `${{ supersedes: "替代", contradicted_by: "冲突", related: "相关" }[kind] || kind} · ${ids.join("、")}`))); detail.append(links);
      }
      detail.append(node("h3", "", "正文 · Markdown 原文"), node("pre", "plain-content", data.body));
    }
    if (window.matchMedia("(max-width: 860px)").matches) detail.scrollIntoView({ behavior: window.matchMedia("(prefers-reduced-motion: reduce)").matches ? "auto" : "smooth", block: "start" });
  } catch (error) {
    if (generation !== viewGeneration || token !== detailGeneration || error.name === "AbortError") return;
    placeholder($("detail"), "暂时无法读取原文", `${error.message} 请重新选择该条目重试。`, "error");
  } finally { if (generation === viewGeneration && token === detailGeneration) $("detail").setAttribute("aria-busy", "false"); }
}
function fillSelect(id, values, selected, emptyLabel) {
  const select = $(id); select.replaceChildren();
  if (!values.length) { const option = node("option", "", emptyLabel); option.value = ""; select.append(option); }
  else values.forEach((value) => { const option = node("option", "", value); option.value = value; select.append(option); });
  select.disabled = !values.length; select.value = values.includes(selected) ? selected : values[0] || "";
  return select.value;
}
async function refreshCatalog() {
  const generation = ++catalogGeneration;
  state.cursor = ""; state.page = 1; clearView();
  placeholder($("list"), "正在刷新目录", "正在读取本地 Project 与 Session…", "loading");
  $("refresh-catalog").disabled = true; $("catalog-error").hidden = true;
  try {
    const catalog = await request("catalog", "/api/catalog");
    if (generation !== catalogGeneration) return;
    state.project = fillSelect("project-select", catalog.projects, state.project, "暂无 Project");
    state.session = fillSelect("session-select", catalog.sessions, state.session, "暂无 Session");
    text("catalog-counts", `${catalog.projects.length} 个项目 · ${catalog.sessions.length} 个会话`); text("data-dir", catalog.data_dir);
  } catch (error) {
    if (generation !== catalogGeneration || error.name === "AbortError") return;
    text("catalog-error", `目录读取失败：${error.message} Global 仍可尝试浏览；检查本地服务后刷新目录。`); $("catalog-error").hidden = false;
    text("catalog-counts", "目录暂不可用");
  } finally {
    if (generation === catalogGeneration) { $("refresh-catalog").disabled = false; loadView(); }
  }
}
document.querySelectorAll("[data-scope]").forEach((button) => button.addEventListener("click", () => { state.scope = button.dataset.scope; loadView(); }));
$("project-select").addEventListener("change", (event) => { state.project = event.target.value; loadView(); });
$("session-select").addEventListener("change", (event) => { state.session = event.target.value; loadView(); });
$("search-form").addEventListener("submit", (event) => { event.preventDefault(); state.query = $("query").value.trim(); loadView(); });
$("status").addEventListener("change", (event) => { state.status = event.target.value; loadView(); });
$("next-page").addEventListener("click", () => { if (nextCursor) { state.cursor = nextCursor; state.page += 1; loadView(false); } });
$("first-page").addEventListener("click", () => loadView());
$("refresh-catalog").addEventListener("click", refreshCatalog);
refreshCatalog();
