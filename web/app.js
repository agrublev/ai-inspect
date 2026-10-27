const state = {
  page: "traces",
  traces: [],
  datasets: [],
  prompts: [],
  detail: null,
  selectedId: null,
  selectedLibraryKey: null,
  editor: null,
  selectedEvent: 0,
  payloadTab: "input",
  detailTab: "timeline",
};

const byId = (id) => document.getElementById(id);
const create = (tag, className, text) => {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
};
const pretty = (value) => value === null || value === undefined ? "—" : typeof value === "string" ? value : JSON.stringify(value, null, 2);

function normalizeStatus(status) {
  if (["error", "failed"].includes(String(status).toLowerCase())) return "error";
  if (["running", "pending"].includes(String(status).toLowerCase())) return "running";
  return "ok";
}

function statusLabel(status) {
  return ({ ok: "Completed", running: "Running", error: "Failed" })[normalizeStatus(status)];
}

function formatTime(value) {
  if (!value) return "—";
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return String(value).replace("T", " ").slice(0, 16);
  return new Intl.DateTimeFormat(undefined, { month: "short", day: "2-digit", hour: "2-digit", minute: "2-digit" }).format(date);
}

function duration(start, end) {
  if (!start || !end) return end ? "—" : "In progress";
  const ms = Math.max(0, new Date(end).getTime() - new Date(start).getTime());
  if (!Number.isFinite(ms)) return "—";
  if (ms < 1000) return `${ms} ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(2)} s`;
  return `${Math.floor(ms / 60_000)}m ${Math.floor((ms % 60_000) / 1000)}s`;
}

function textValue(value) {
  if (typeof value === "string") return value;
  if (Array.isArray(value)) return value.map(textValue).filter(Boolean).join(" ");
  if (value && typeof value === "object") {
    if (typeof value.text === "string") return value.text;
    if (Array.isArray(value.content)) return textValue(value.content);
  }
  return value === null || value === undefined ? "" : JSON.stringify(value);
}

function short(value, max = 110) {
  const text = textValue(value).replace(/\s+/g, " ").trim();
  return text.length > max ? `${text.slice(0, max - 1)}…` : text || "(empty input)";
}

function showToast(message) {
  const toast = byId("toast");
  toast.textContent = message;
  toast.classList.add("visible");
  window.clearTimeout(showToast.timeout);
  showToast.timeout = window.setTimeout(() => toast.classList.remove("visible"), 1500);
}

function currentLibraryEntries() {
  return state.page === "datasets" ? state.datasets : state.prompts;
}

function libraryEntryKey(entry) {
  return state.page === "datasets" ? entry.name : `${entry.name}\u0000${entry.version}`;
}

function visibleLibraryEntries() {
  const query = byId("library-search").value.trim().toLowerCase();
  return currentLibraryEntries().filter((entry) => {
    const text = [entry.name, entry.description, entry.template, entry.version, entry.items, entry.created_at].join(" ").toLowerCase();
    return !query || text.includes(query);
  });
}

function renderLibraryRows() {
  const entries = visibleLibraryEntries();
  const rows = byId("library-rows");
  rows.replaceChildren();
  byId("library-count").textContent = String(entries.length);
  byId("library-summary").textContent = entries.length
    ? `${entries.length} ${state.page === "datasets" ? "dataset" : "prompt version"}${entries.length === 1 ? "" : "s"}`
    : "No matching items";
  byId("library-empty").hidden = entries.length !== 0;
  if (!entries.length) {
    byId("library-empty-title").textContent = currentLibraryEntries().length ? "No matches" : state.page === "datasets" ? "No datasets yet" : "No prompt versions yet";
    byId("library-empty-copy").textContent = currentLibraryEntries().length
      ? "Try another search term."
      : state.page === "datasets"
        ? "Create a dataset with the iris CLI to browse its examples here."
        : "Save a prompt version with the iris CLI to review it here.";
  }
  for (const entry of entries) {
    const key = libraryEntryKey(entry);
    const row = create("button", `library-row${key === state.selectedLibraryKey ? " selected" : ""}`);
    row.type = "button";
    row.setAttribute("aria-pressed", String(key === state.selectedLibraryKey));
    const main = create("span", "library-row-main");
    main.append(create("strong", "library-row-title", entry.name));
    const subtitle = state.page === "datasets"
      ? entry.description || "No description"
      : `Version v${entry.version}`;
    main.append(create("span", "library-row-subtitle", subtitle));
    const meta = state.page === "datasets"
      ? `${Number(entry.items || 0).toLocaleString()} example${Number(entry.items || 0) === 1 ? "" : "s"}`
      : formatTime(entry.created_at);
    row.append(main, create("span", "library-row-meta", meta));
    row.addEventListener("click", () => selectLibraryEntry(entry));
    rows.append(row);
  }
}

function renderLibraryHeader() {
  const datasets = state.page === "datasets";
  const title = datasets ? "Datasets" : "Prompts";
  byId("page-breadcrumb").textContent = title;
  byId("library-title").textContent = title;
  byId("library-description").textContent = datasets
    ? "Browse evaluation examples and expected outputs."
    : "Review immutable prompt templates and their version history.";
  byId("library-list-title").firstChild.textContent = datasets ? "Datasets " : "Prompt versions ";
  byId("library-list-description").textContent = datasets
    ? "Evaluation inputs and expected outputs"
    : "Saved templates, grouped by name and version";
  byId("library-search").setAttribute("aria-label", `Search ${title.toLowerCase()}`);
  byId("library-search").placeholder = datasets ? "Search datasets…" : "Search prompts or template text…";
  byId("library-create-button").textContent = datasets ? "Create dataset" : "Create prompt";
  byId("library-create-button").setAttribute("aria-label", datasets ? "Create dataset" : "Create prompt");
  byId("refresh-button").setAttribute("aria-label", `Refresh ${title.toLowerCase()}`);
  byId("refresh-button").title = `Refresh ${title.toLowerCase()}`;
  document.querySelectorAll(".nav-item[data-page]").forEach((item) => {
    const active = item.dataset.page === state.page;
    item.classList.toggle("active", active);
    if (active) item.setAttribute("aria-current", "page");
    else item.removeAttribute("aria-current");
  });
}

function clearLibraryDetail() {
  state.selectedLibraryKey = null;
  byId("library-detail-empty").hidden = false;
  byId("library-detail-content").hidden = true;
  byId("library-detail-content").replaceChildren();
  byId("library-detail-empty-title").textContent = currentLibraryEntries().length ? "Select an item" : "Nothing to inspect";
  byId("library-detail-empty-copy").textContent = currentLibraryEntries().length
    ? "Choose a dataset or prompt version to inspect its contents."
    : "Create local data with the iris CLI, then it will appear here.";
  renderLibraryRows();
}

async function selectLibraryEntry(entry) {
  const page = state.page;
  const key = libraryEntryKey(entry);
  state.selectedLibraryKey = key;
  renderLibraryRows();
  try {
    let detail = entry;
    if (page === "datasets") {
      const response = await fetch(`/api/dataset?name=${encodeURIComponent(entry.name)}`, { cache: "no-store" });
      if (!response.ok) throw new Error(`Could not load dataset (${response.status})`);
      detail = await response.json();
    }
    if (page !== state.page || key !== state.selectedLibraryKey) return;
    byId("library-detail-empty").hidden = true;
    byId("library-detail-content").hidden = false;
    byId("library-detail-content").replaceChildren();
    if (page === "datasets") renderDatasetDetail(detail);
    else renderPromptDetail(detail);
  } catch (error) {
    if (page === state.page && key === state.selectedLibraryKey) {
      showToast(error.message || "Unable to load local data");
    }
  }
}

function detailHeader(eyebrow, title, subtitle, meta) {
  const header = create("header", "library-detail-header");
  header.append(create("div", "eyebrow", eyebrow), create("h2", "", title));
  if (subtitle) header.append(create("p", "library-detail-description", subtitle));
  if (meta) header.append(create("div", "library-detail-meta", meta));
  return header;
}

function valueCard(label, value) {
  const card = create("section", "library-value-card");
  card.append(create("h3", "", label), create("pre", "", value === null || value === undefined ? "Not set" : pretty(value)));
  return card;
}

function renderDatasetDetail(detail) {
  const dataset = detail.dataset;
  const items = Array.isArray(detail.items) ? detail.items : [];
  const content = byId("library-detail-content");
  const header = detailHeader("EVALUATION DATASET", dataset.name, dataset.description, `${items.length.toLocaleString()} example${items.length === 1 ? "" : "s"} · Created ${formatTime(dataset.created_at)}`);
  const actions = create("div", "library-detail-actions");
  actions.append(editorAction("Edit dataset", "button button-secondary", () => openEditor("dataset", dataset)));
  actions.append(editorAction("Add example", "button button-primary", () => openEditor("item", { dataset })));
  header.append(actions);
  content.append(header);
  const body = create("div", "library-detail-body");
  body.append(create("div", "section-label", "Examples"));
  if (!items.length) {
    body.append(create("div", "detail-empty-inline", "This dataset has no examples yet."));
  }
  for (const [index, item] of items.entries()) {
    const example = create("article", "dataset-example");
    const heading = create("div", "dataset-example-heading");
    heading.append(create("strong", "", `Example ${index + 1}`), create("span", "", `#${item.item_id}`));
    heading.append(editorAction("Edit", "dataset-example-edit", () => openEditor("item", { dataset, item })));
    example.append(heading);
    const values = create("div", "dataset-values");
    values.append(valueCard("Input", item.input), valueCard("Expected output", item.expected));
    example.append(values);
    if (item.metadata && Object.keys(item.metadata).length) example.append(valueCard("Metadata", item.metadata));
    body.append(example);
  }
  content.append(body);
}

function renderPromptDetail(prompt) {
  const content = byId("library-detail-content");
  const header = detailHeader("PROMPT VERSION", prompt.name, `Version v${prompt.version}`, `Saved ${formatTime(prompt.created_at)} · Immutable local version`);
  const actions = create("div", "library-detail-actions");
  actions.append(editorAction("Create new version", "button button-primary", () => openEditor("prompt", prompt)));
  header.append(actions);
  content.append(header);
  const body = create("div", "library-detail-body prompt-detail-body");
  body.append(create("div", "section-label", "Template"));
  const template = create("pre", "prompt-template", prompt.template || "");
  template.tabIndex = 0;
  body.append(template);
  content.append(body);
}

function editorAction(label, className, onClick) {
  const button = create("button", className, label);
  button.type = "button";
  button.addEventListener("click", onClick);
  return button;
}

function editorField(label, name, value, options = {}) {
  const field = create("label", "editor-field");
  field.append(create("span", "", label));
  const control = create(options.multiline ? "textarea" : "input", options.className || "");
  control.id = `editor-${name}`;
  control.name = name;
  control.value = value ?? "";
  control.required = Boolean(options.required);
  if (options.maxLength) control.maxLength = options.maxLength;
  if (!options.multiline) control.type = "text";
  if (options.disabled) control.disabled = true;
  if (options.placeholder) control.placeholder = options.placeholder;
  field.append(control);
  if (options.hint) field.append(create("small", "", options.hint));
  byId("editor-fields").append(field);
  return control;
}

function openEditor(kind, record = null) {
  const dialog = byId("editor-dialog");
  const fields = byId("editor-fields");
  fields.replaceChildren();
  byId("editor-error").hidden = true;
  state.editor = { kind, record };

  let firstField;
  if (kind === "dataset") {
    const editing = Boolean(record);
    byId("editor-eyebrow").textContent = "EVALUATION DATASET";
    byId("editor-title").textContent = editing ? "Edit dataset" : "Create dataset";
    byId("editor-help").textContent = editing
      ? "Rename this dataset or update its description. Existing examples stay attached."
      : "Create a local dataset to organize inputs and expected outputs.";
    firstField = editorField("Dataset name", "name", record?.name || "", { required: true, maxLength: 200, placeholder: "e.g. support quality" });
    editorField("Description", "description", record?.description || "", { multiline: true, maxLength: 5000, placeholder: "What is this dataset for?" });
    byId("editor-save").textContent = editing ? "Save changes" : "Create dataset";
  } else if (kind === "item") {
    const { dataset, item } = record;
    byId("editor-eyebrow").textContent = dataset.name;
    byId("editor-title").textContent = item ? "Edit example" : "Add example";
    byId("editor-help").textContent = "Enter valid JSON. Expected output is optional; example metadata is preserved when editing.";
    firstField = editorField("Input JSON", "input", item ? JSON.stringify(item.input, null, 2) : "", {
      multiline: true,
      className: "json-input",
      required: true,
      placeholder: '{\n  "question": "..."\n}',
    });
    editorField("Expected output JSON (optional)", "expected", item?.expected == null ? "" : JSON.stringify(item.expected, null, 2), {
      multiline: true,
      className: "json-input",
      placeholder: '{\n  "answer": "..."\n}',
    });
    byId("editor-save").textContent = item ? "Save example" : "Add example";
  } else {
    const editing = Boolean(record);
    byId("editor-eyebrow").textContent = "PROMPT VERSION";
    byId("editor-title").textContent = editing ? "Create new version" : "Create prompt";
    byId("editor-help").textContent = editing
      ? "Saving creates the next immutable version. Previous versions remain unchanged."
      : "Save a prompt template locally. You can add versions without overwriting history.";
    firstField = editorField("Prompt name", "name", record?.name || "", {
      required: true,
      maxLength: 200,
      placeholder: "e.g. support assistant",
      disabled: Boolean(record),
      hint: record ? "Prompt identity stays fixed across its version history." : "",
    });
    const templateField = editorField("Template", "template", record?.template || "", {
      multiline: true,
      className: "prompt-input",
      required: true,
      placeholder: "Write the prompt template…",
    });
    if (record) firstField = templateField;
    byId("editor-save").textContent = editing ? "Save new version" : "Create prompt";
  }

  dialog.showModal();
  firstField.focus();
}

function editorJson(name, label, required) {
  const text = byId(`editor-${name}`).value.trim();
  if (!text) {
    if (required) throw new Error(`${label} is required.`);
    return null;
  }
  try {
    return JSON.parse(text);
  } catch {
    throw new Error(`${label} must be valid JSON.`);
  }
}

async function saveEditor(event) {
  event.preventDefault();
  const editor = state.editor;
  if (!editor) return;
  const saveButton = byId("editor-save");
  const errorBox = byId("editor-error");
  saveButton.disabled = true;
  errorBox.hidden = true;
  try {
    let endpoint;
    let method;
    let payload;
    if (editor.kind === "dataset") {
      const name = byId("editor-name").value.trim();
      if (!name) throw new Error("Dataset name is required.");
      payload = { name, description: byId("editor-description").value };
      endpoint = editor.record
        ? `/api/dataset?name=${encodeURIComponent(editor.record.name)}`
        : "/api/datasets";
      method = editor.record ? "PUT" : "POST";
    } else if (editor.kind === "item") {
      const input = editorJson("input", "Input", true);
      const expected = editorJson("expected", "Expected output", false);
      const dataset = editor.record.dataset;
      endpoint = `/api/dataset-item?name=${encodeURIComponent(dataset.name)}`;
      method = editor.record.item ? "PUT" : "POST";
      if (editor.record.item) endpoint += `&id=${editor.record.item.item_id}`;
      payload = { input, expected };
    } else {
      const name = byId("editor-name").value.trim();
      const template = byId("editor-template").value;
      if (!name) throw new Error("Prompt name is required.");
      if (!template.trim()) throw new Error("Prompt template is required.");
      endpoint = "/api/prompts";
      method = "POST";
      payload = { name, template };
    }

    const response = await fetch(endpoint, {
      method,
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(payload),
      cache: "no-store",
    });
    const data = await response.json();
    if (!response.ok) throw new Error(data.error || `Save failed (${response.status})`);

    if (editor.kind === "dataset") state.selectedLibraryKey = data.dataset.name;
    else if (editor.kind === "item") state.selectedLibraryKey = editor.record.dataset.name;
    else state.selectedLibraryKey = `${data.name}\u0000${data.version}`;
    byId("editor-dialog").close();
    showToast(editor.kind === "prompt" ? `Saved ${data.name} v${data.version}` : "Saved locally");
    await loadLibrary();
  } catch (error) {
    errorBox.textContent = error.message || "Could not save changes.";
    errorBox.hidden = false;
  } finally {
    saveButton.disabled = false;
  }
}

async function loadLibrary() {
  const page = state.page;
  const previousKey = state.selectedLibraryKey;
  const endpoint = page === "datasets" ? "/api/datasets" : "/api/prompts";
  const property = page === "datasets" ? "datasets" : "prompts";
  try {
    const response = await fetch(endpoint, { cache: "no-store" });
    if (!response.ok) throw new Error(`Could not load ${page} (${response.status})`);
    const data = await response.json();
    if (state.page !== page) return;
    state[property] = Array.isArray(data[property]) ? data[property] : [];
    const badgeId = page === "datasets" ? "nav-dataset-count" : "nav-prompt-count";
    byId(badgeId).textContent = String(state[property].length);
    byId("library-updated").textContent = `Updated ${new Intl.DateTimeFormat(undefined, { hour: "2-digit", minute: "2-digit", second: "2-digit" }).format(new Date())}`;
    renderLibraryRows();
    const selected = state[property].find((entry) => libraryEntryKey(entry) === previousKey) || state[property][0];
    if (selected) await selectLibraryEntry(selected);
    else clearLibraryDetail();
  } catch (error) {
    if (state.page !== page) return;
    state[property] = [];
    byId("library-updated").textContent = "Local database unavailable";
    clearLibraryDetail();
    byId("library-rows").append(create("div", "library-load-error", error.message || `Unable to load ${page}`));
    byId("library-empty").hidden = true;
  }
}

async function loadLibraryCounts() {
  for (const [endpoint, property, badgeId] of [
    ["/api/datasets", "datasets", "nav-dataset-count"],
    ["/api/prompts", "prompts", "nav-prompt-count"],
  ]) {
    try {
      const response = await fetch(endpoint, { cache: "no-store" });
      if (!response.ok) continue;
      const data = await response.json();
      const entries = Array.isArray(data[property]) ? data[property] : [];
      state[property] = entries;
      byId(badgeId).textContent = String(entries.length);
    } catch {
      // Keep the badge neutral when local storage is not available yet.
    }
  }
}

function navigateToPage() {
  const requestedPage = window.location.hash.slice(1);
  const nextPage = ["datasets", "prompts"].includes(requestedPage) ? requestedPage : "traces";
  if (nextPage !== state.page) byId("library-search").value = "";
  state.page = nextPage;
  const traces = state.page === "traces";
  byId("trace-page").hidden = !traces;
  byId("library-page").hidden = traces;
  if (traces) {
    byId("page-breadcrumb").textContent = "Traces";
    byId("refresh-button").setAttribute("aria-label", "Refresh traces");
    byId("refresh-button").title = "Refresh traces";
    document.querySelectorAll(".nav-item[data-page]").forEach((item) => {
      const active = item.dataset.page === "traces";
      item.classList.toggle("active", active);
      if (active) item.setAttribute("aria-current", "page");
      else item.removeAttribute("aria-current");
    });
    loadTraces();
  } else {
    renderLibraryHeader();
    loadLibrary();
  }
}

async function loadTraces(keepSelection = true) {
  const previousId = keepSelection ? state.selectedId : null;
  try {
    const response = await fetch("/api/traces", { cache: "no-store" });
    if (!response.ok) throw new Error(`Could not load traces (${response.status})`);
    const data = await response.json();
    state.traces = Array.isArray(data.traces) ? data.traces : [];
    byId("updated-label").textContent = `Updated ${new Intl.DateTimeFormat(undefined, { hour: "2-digit", minute: "2-digit", second: "2-digit" }).format(new Date())}`;
    renderStats();
    renderRows();
    const selected = state.traces.find((trace) => trace.trace_id === previousId) || state.traces[0];
    if (selected) await loadDetail(selected.trace_id, true);
    else clearDetail();
  } catch (error) {
    byId("updated-label").textContent = "Local database unavailable";
    byId("trace-rows").replaceChildren();
    const row = create("tr");
    const cell = create("td", "loading-cell", error.message || "Unable to load traces");
    cell.colSpan = 5;
    row.append(cell);
    byId("trace-rows").append(row);
  }
}

async function importStashFile(event) {
  const input = event.currentTarget;
  const file = input.files?.[0];
  input.value = "";
  if (!file) return;
  if (file.size > 16 * 1024 * 1024) {
    showToast("Stash import must be 16 MiB or smaller");
    return;
  }
  try {
    const response = await fetch("/api/traces/import", {
      method: "POST",
      headers: { "Content-Type": "application/x-ndjson" },
      body: await file.text(),
      cache: "no-store",
    });
    const result = await response.json();
    if (!response.ok) throw new Error(result.error || `Import failed (${response.status})`);
    showToast(`Imported ${result.imported} Stash trace${result.imported === 1 ? "" : "s"}`);
    await loadTraces(false);
  } catch (error) {
    showToast(error.message || "Could not import Stash traces");
  }
}

async function exportStashTraces() {
  const button = byId("export-stash-button");
  button.disabled = true;
  try {
    const response = await fetch("/api/traces/export?format=stash", { cache: "no-store" });
    if (!response.ok) {
      const result = await response.json();
      throw new Error(result.error || `Export failed (${response.status})`);
    }
    const url = URL.createObjectURL(await response.blob());
    const link = create("a");
    link.href = url;
    link.download = `iris-traces-${new Date().toISOString().slice(0, 10)}.stash.jsonl`;
    document.body.append(link);
    link.click();
    link.remove();
    URL.revokeObjectURL(url);
    showToast("Exported Stash trace JSONL");
  } catch (error) {
    showToast(error.message || "Could not export Stash traces");
  } finally {
    button.disabled = false;
  }
}

function renderStats() {
  const traces = state.traces;
  const failed = traces.filter((trace) => normalizeStatus(trace.status) === "error").length;
  const events = traces.reduce((sum, trace) => sum + Number(trace.event_count || 0), 0);
  const active = traces.filter((trace) => normalizeStatus(trace.status) === "running").length;
  byId("stat-total").textContent = traces.length.toLocaleString();
  byId("stat-failed").textContent = failed.toLocaleString();
  byId("stat-events").textContent = events.toLocaleString();
  byId("stat-active").textContent = active.toLocaleString();
  byId("stat-failed-foot").textContent = failed ? `${failed} run${failed === 1 ? "" : "s"} need attention` : "No failed runs";
  byId("nav-trace-count").textContent = traces.length > 999 ? "999+" : traces.length;

  const select = byId("type-filter");
  const current = select.value;
  const kinds = new Set();
  for (const trace of traces) {
    if (Array.isArray(trace.event_types)) for (const kind of trace.event_types) kinds.add(kind);
  }
  select.replaceChildren(new Option("All events", "all"), ...[...kinds].sort().map((kind) => new Option(kind, kind)));
  select.value = kinds.has(current) ? current : "all";
}

function visibleTraces() {
  const query = byId("search-input").value.trim().toLowerCase();
  const status = byId("status-filter").value;
  const eventType = byId("type-filter").value;
  return state.traces.filter((trace) => {
    const normalized = normalizeStatus(trace.status);
    if (status !== "all" && normalized !== status) return false;
    if (eventType !== "all" && !trace.event_types?.includes(eventType)) return false;
    const haystack = [trace.trace_id, trace.session_id, trace.name, textValue(trace.input)].join(" ").toLowerCase();
    return !query || haystack.includes(query);
  });
}

function renderRows(autoSelect = false) {
  const traces = visibleTraces();
  const body = byId("trace-rows");
  body.replaceChildren();
  byId("result-count").textContent = String(traces.length);
  byId("table-summary").textContent = traces.length ? `Showing ${traces.length} trace${traces.length === 1 ? "" : "s"}` : "No matching traces";
  byId("empty-state").hidden = traces.length !== 0;
  body.closest("table").hidden = traces.length === 0;

  for (const trace of traces) {
    const row = create("tr", trace.trace_id === state.selectedId ? "selected" : "");
    row.tabIndex = 0;
    row.setAttribute("aria-label", `Open trace ${trace.trace_id}`);
    const first = create("td");
    const traceLabel = trace.name && trace.name !== "User prompt" ? trace.name : `Session ${trace.session_id}`;
    first.append(create("div", "trace-primary", short(trace.input, 56)), create("div", "trace-secondary", `${traceLabel} · ${trace.trace_id}`));
    const statusCell = create("td");
    statusCell.append(create("span", `status-pill status-${normalizeStatus(trace.status)}`, statusLabel(trace.status)));
    const count = create("td");
    count.append(create("span", "event-count", Number(trace.event_count || 0).toLocaleString()));
    const started = create("td");
    started.append(create("span", "started-at", formatTime(trace.started_at)));
    const arrow = create("td", "row-arrow", "›");
    row.append(first, statusCell, count, started, arrow);
    row.addEventListener("click", () => loadDetail(trace.trace_id, false, true));
    row.addEventListener("keydown", (event) => {
      if (event.key === "Enter" || event.key === " ") {
        event.preventDefault();
        loadDetail(trace.trace_id, false, true);
      }
    });
    body.append(row);
  }
  if (autoSelect && traces.length && !traces.some((trace) => trace.trace_id === state.selectedId)) {
    loadDetail(traces[0].trace_id);
  }
}

function clearDetail() {
  state.selectedId = null;
  state.detail = null;
  byId("detail-empty").hidden = false;
  byId("detail-content").hidden = true;
  renderRows();
}

async function loadDetail(traceId, preserveView = false, scrollToDetails = false) {
  const sameTrace = state.selectedId === traceId;
  state.selectedId = traceId;
  renderRows();
  try {
    const response = await fetch(`/api/trace?id=${encodeURIComponent(traceId)}`, { cache: "no-store" });
    if (!response.ok) throw new Error(`Trace request failed (${response.status})`);
    state.detail = await response.json();
    if (!preserveView || !sameTrace) {
      state.selectedEvent = 0;
      state.payloadTab = "input";
      state.detailTab = "timeline";
    }
    byId("detail-empty").hidden = true;
    byId("detail-content").hidden = false;
    renderDetail();
    if (scrollToDetails && window.matchMedia("(max-width: 700px)").matches) {
      requestAnimationFrame(() => byId("detail-panel").scrollIntoView({ behavior: "smooth", block: "start" }));
    }
  } catch (error) {
    showToast(error.message || "Unable to load trace detail");
  }
}

function renderDetail() {
  const detail = state.detail;
  if (!detail) return;
  const trace = detail.trace;
  const events = detail.events || [];
  const spans = detail.spans || [];
  const traceName = trace.name && trace.name !== "User prompt" ? trace.name : short(trace.input, 70);
  byId("detail-name").textContent = traceName || "Trace";
  byId("detail-id").textContent = trace.trace_id;
  const status = byId("detail-status");
  status.className = `status-pill status-${normalizeStatus(trace.status)}`;
  status.textContent = statusLabel(trace.status);
  byId("detail-meta").replaceChildren();
  const usage = events.reduce((total, event) => {
    const eventUsage = event.metadata?.usage || {};
    total.input += Number(eventUsage.input_tokens || 0);
    total.output += Number(eventUsage.output_tokens || 0);
    return total;
  }, { input: 0, output: 0 });
  const evaluations = detail.evaluations || [];
  const averageScore = evaluations.length
    ? evaluations.reduce((sum, evaluation) => sum + Number(evaluation.score || 0), 0) / evaluations.length
    : null;
  const model = events.find((event) => event.metadata?.model)?.metadata.model;
  const meta = [["Started", formatTime(trace.started_at)], ["Duration", duration(trace.started_at, trace.ended_at)], ["Session", trace.session_id]];
  if (model) meta.push(["Model", model]);
  if (usage.input || usage.output) meta.push(["Tokens", `${(usage.input + usage.output).toLocaleString()} (${usage.input.toLocaleString()} in · ${usage.output.toLocaleString()} out)`]);
  if (averageScore !== null) meta.push(["Eval avg", `${averageScore.toFixed(2)} · ${evaluations.length} score${evaluations.length === 1 ? "" : "s"}`]);
  for (const [label, value] of meta) {
    const item = create("span");
    item.append(create("b", "", `${label}:`), document.createTextNode(` ${value}`));
    byId("detail-meta").append(item);
  }
  byId("event-tab-count").textContent = String(events.length);
  byId("span-tab-count").textContent = String(spans.length);

  document.querySelectorAll(".detail-tab").forEach((tab) => {
    const active = tab.dataset.tab === state.detailTab;
    tab.classList.toggle("active", active);
    tab.setAttribute("aria-selected", String(active));
  });
  const body = byId("detail-body");
  body.replaceChildren();
  if (state.detailTab === "timeline") renderTimeline(body, trace, events);
  else if (state.detailTab === "spans") renderSpans(body, spans);
  else body.append(create("pre", "json-view", JSON.stringify(detail, null, 2)));
}

function renderOverview(target, trace) {
  const grid = create("div", "io-grid");
  for (const [label, value] of [["Input", trace.input], ["Output", trace.output]]) {
    const card = create("section", "io-card");
    card.append(create("h4", "", label), create("pre", "", value === undefined || value === null ? "No captured data" : pretty(value)));
    grid.append(card);
  }
  target.append(grid);
}

function renderTimeline(target, trace, events) {
  renderOverview(target, trace);
  target.append(create("div", "section-label", "Execution timeline"));
  const list = create("div", "timeline-list");
  const filtered = byId("type-filter").value === "all" ? events : events.filter((event) => event.event_type === byId("type-filter").value);
  if (filtered.length === 0) list.append(create("div", "detail-empty-inline", "No events match this filter."));
  for (const event of filtered) {
    const originalIndex = events.indexOf(event);
    const item = create("button", `timeline-item${originalIndex === state.selectedEvent ? " selected" : ""}`);
    item.type = "button";
    const kind = String(event.event_type || "").toLowerCase();
    const dotKind = event.status === "error" || event.status === "failed" ? "error" : kind.includes("tool") ? "tool" : kind.includes("agent") ? "agent" : "";
    item.append(create("span", `timeline-dot ${dotKind}`));
    const main = create("span", "event-main");
    main.append(create("span", "event-name", event.name || event.event_type || "Event"), create("span", "event-type", event.event_type || "event"));
    item.append(main, create("span", "event-time", formatTime(event.timestamp)));
    item.addEventListener("click", () => {
      state.selectedEvent = originalIndex;
      state.payloadTab = "input";
      renderDetail();
    });
    list.append(item);
  }
  target.append(list);
  const selectedItem = list.querySelector(".timeline-item.selected");
  if (selectedItem) {
    const listBounds = list.getBoundingClientRect();
    const itemBounds = selectedItem.getBoundingClientRect();
    if (itemBounds.bottom > listBounds.bottom) list.scrollTop += itemBounds.bottom - listBounds.bottom;
    else if (itemBounds.top < listBounds.top) list.scrollTop -= listBounds.top - itemBounds.top;
  }
  renderEventInspector(target, events[state.selectedEvent]);
}

function renderEventInspector(target, event) {
  if (!event) return;
  const inspector = create("section", "event-inspector");
  const heading = create("div", "inspector-heading");
  heading.append(create("strong", "", event.name || event.event_type || "Event"));
  const tabs = create("div", "payload-tabs");
  for (const tab of ["input", "output", "metadata", "raw"]) {
    const button = create("button", `payload-tab${state.payloadTab === tab ? " active" : ""}`, tab);
    button.type = "button";
    button.addEventListener("click", () => {
      state.payloadTab = tab;
      renderDetail();
    });
    tabs.append(button);
  }
  heading.append(tabs);
  inspector.append(heading);
  const payload = create("div", "event-payload");
  const value = state.payloadTab === "raw" ? event : event[state.payloadTab];
  if (value === undefined || value === null) payload.append(create("div", "payload-empty", `No ${state.payloadTab} recorded for this event.`));
  else payload.append(create("pre", "", pretty(value)));
  inspector.append(payload);
  target.append(inspector);
}

function spanDepth(span, spans) {
  let depth = 0;
  let parentId = span.parent_span_id;
  const visited = new Set([span.span_id]);
  while (parentId && depth < 4 && !visited.has(parentId)) {
    visited.add(parentId);
    const parent = spans.find((item) => item.span_id === parentId);
    if (!parent) break;
    parentId = parent.parent_span_id;
    depth += 1;
  }
  return depth;
}

function renderSpans(target, spans) {
  target.append(create("div", "section-label", "Span hierarchy"));
  if (!spans.length) {
    target.append(create("div", "detail-empty-inline", "No spans recorded for this trace."));
    return;
  }
  const list = create("div", "span-list");
  for (const span of spans) {
    const kind = String(span.kind || "span").toLowerCase();
    const row = create("button", `span-row indent-${spanDepth(span, spans)}`);
    row.type = "button";
    row.append(create("span", `span-kind ${kind}`, kind), create("span", "span-name", span.name || kind), create("span", "span-time", duration(span.started_at, span.ended_at)));
    row.addEventListener("click", () => {
      const match = (state.detail.events || []).findIndex((event) => event.span_id === span.span_id);
      if (match >= 0) {
        state.detailTab = "timeline";
        state.selectedEvent = match;
        state.payloadTab = "input";
        renderDetail();
      } else {
        const content = create("div", "event-inspector");
        const heading = create("div", "inspector-heading", span.name || kind);
        const payload = create("div", "event-payload");
        payload.append(create("pre", "", JSON.stringify(span, null, 2)));
        content.append(heading, payload);
        list.querySelector(".event-inspector")?.remove();
        list.append(content);
      }
    });
    list.append(row);
  }
  target.append(list);
}

byId("search-input").addEventListener("input", () => renderRows(true));
byId("status-filter").addEventListener("change", () => renderRows(true));
byId("type-filter").addEventListener("change", () => renderRows(true));
byId("clear-filters").addEventListener("click", () => {
  byId("search-input").value = "";
  byId("status-filter").value = "all";
  byId("type-filter").value = "all";
  renderRows(true);
});
byId("library-search").addEventListener("input", renderLibraryRows);
byId("library-create-button").addEventListener("click", () => {
  if (state.page === "datasets") openEditor("dataset");
  else openEditor("prompt");
});
byId("editor-form").addEventListener("submit", saveEditor);
byId("editor-close").addEventListener("click", () => byId("editor-dialog").close());
byId("editor-cancel").addEventListener("click", () => byId("editor-dialog").close());
byId("editor-dialog").addEventListener("close", () => {
  state.editor = null;
});
byId("refresh-button").addEventListener("click", () => state.page === "traces" ? loadTraces() : loadLibrary());
byId("refresh-heading").addEventListener("click", () => loadTraces());
byId("import-stash-button").addEventListener("click", () => byId("stash-import-file").click());
byId("stash-import-file").addEventListener("change", importStashFile);
byId("export-stash-button").addEventListener("click", exportStashTraces);
byId("copy-trace-id").addEventListener("click", async () => {
  if (!state.detail?.trace?.trace_id) return;
  try {
    await navigator.clipboard.writeText(state.detail.trace.trace_id);
    showToast("Trace ID copied");
  } catch {
    showToast("Clipboard access is unavailable");
  }
});
document.querySelectorAll(".detail-tab").forEach((tab) => tab.addEventListener("click", () => {
  state.detailTab = tab.dataset.tab;
  renderDetail();
}));

window.addEventListener("hashchange", navigateToPage);
loadLibraryCounts();
navigateToPage();
window.setInterval(() => state.page === "traces" ? loadTraces() : loadLibrary(), 5000);
