use axum::{
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Redirect, Response},
};

const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'";

pub async fn root() -> Redirect {
    Redirect::permanent("/admin/")
}

pub async fn index() -> Response {
    asset("text/html; charset=utf-8", INDEX)
}

pub async fn script() -> Response {
    asset("text/javascript; charset=utf-8", SCRIPT)
}

pub async fn style() -> Response {
    asset("text/css; charset=utf-8", STYLE)
}

fn asset(content_type: &'static str, body: &'static str) -> Response {
    let mut response = (StatusCode::OK, body).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

const INDEX: &str = r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width,initial-scale=1">
  <title>ModelKeep administration</title>
  <link rel="stylesheet" href="/admin/style.css">
</head>
<body>
  <header><div><p class="eyebrow">Archive control plane</p><h1>ModelKeep</h1></div><p id="connection" role="status">Connecting…</p></header>
  <main>
    <section id="auth" class="panel authentication" hidden>
      <div><h2>Authentication required</h2><p>Enter the management token. It is kept only for this browser tab.</p></div>
      <form id="auth-form"><label for="token">Bearer token</label><div class="inline"><input id="token" type="password" autocomplete="current-password" required><button>Connect</button></div></form>
    </section>

    <section aria-labelledby="overview-title">
      <div class="section-heading"><div><p class="eyebrow">At a glance</p><h2 id="overview-title">Overview</h2></div><button id="refresh" class="secondary">Refresh</button></div>
      <div id="overview" class="metrics" aria-live="polite"><p class="empty">Waiting for the service…</p></div>
    </section>

    <section class="grid">
      <div class="panel">
        <div class="section-heading"><div><p class="eyebrow">Durable state</p><h2>Repositories</h2></div></div>
        <div id="repositories" class="list" aria-live="polite"><p class="empty">No data loaded.</p></div>
      </div>
      <div class="panel">
        <p class="eyebrow">Selected repository</p><h2 id="detail-title">Details</h2>
        <div id="repository-detail" class="detail"><p class="empty">Choose a repository to inspect revisions and refs.</p></div>
      </div>
    </section>

    <section class="grid operations">
      <div class="panel">
        <p class="eyebrow">Archive operation</p><h2>Start a job</h2>
        <form id="job-form">
          <label for="kind">Operation</label><select id="kind"><option value="prefetch">Prefetch</option><option value="refresh">Refresh ref</option><option value="verify">Verify revision</option><option value="audit">Audit archive</option></select>
          <div id="target-fields"><label for="repo-type">Repository type</label><select id="repo-type"><option value="model">Model</option><option value="dataset">Dataset</option></select><label for="repo-id">Repository</label><input id="repo-id" placeholder="namespace/repository" required><label for="revision">Revision or ref</label><input id="revision" value="main" required></div>
          <div id="selection-fields"><label for="include">Include patterns</label><textarea id="include" rows="2" placeholder="one per line, e.g. Qwen3-Coder-Next-Q4_K_M/*"></textarea><label for="exclude">Exclude patterns</label><textarea id="exclude" rows="2" placeholder="one per line; leave both empty for the whole repository"></textarea></div>
          <button id="submit-job">Start job</button>
        </form>
        <p id="form-message" role="status"></p>
      </div>
      <div class="panel jobs-panel">
        <div class="section-heading"><div><p class="eyebrow">Persistent operations</p><h2>Recent jobs</h2></div><span>latest 50</span></div>
        <div id="jobs" class="list" aria-live="polite"><p class="empty">No jobs loaded.</p></div>
        <button id="more-jobs" class="secondary" hidden>Load older jobs</button>
      </div>
    </section>

    <section aria-labelledby="acquisitions-title">
      <div class="section-heading"><div><p class="eyebrow">Upstream transfers</p><h2 id="acquisitions-title">Acquisitions in flight</h2></div><span id="slots">—</span></div>
      <div id="acquisitions" class="list" aria-live="polite"><p class="empty">No acquisitions loaded.</p></div>
    </section>
  </main>
  <script src="/admin/app.js" defer></script>
</body>
</html>"#;

const SCRIPT: &str = r#"'use strict';
const $ = (id) => document.getElementById(id);
let token = sessionStorage.getItem('modelkeep-admin-token') || '';
let timer;
let jobsCursor = null;
let jobsExpanded = false;
const progressSamples = new Map();

function headers(write = false) {
  const result = {Accept: 'application/json'};
  if (token) result.Authorization = `Bearer ${token}`;
  if (write) {
    result['Content-Type'] = 'application/json';
    result['X-ModelKeep-CSRF'] = '1';
    result['Idempotency-Key'] = crypto.randomUUID();
  }
  return result;
}

async function api(path, options = {}) {
  const response = await fetch(path, {...options, headers: {...headers(Boolean(options.body)), ...(options.headers || {})}});
  if (response.status === 401) {
    const methods = (response.headers.get('x-modelkeep-auth-methods') || '').split(',');
    const bearerAvailable = methods.includes('bearer');
    $('auth').hidden = !bearerAvailable;
    $('connection').textContent = bearerAvailable ? 'Authentication required' : 'Tailscale authorization required';
    if (token) { token = ''; sessionStorage.removeItem('modelkeep-admin-token'); }
    throw new Error(bearerAvailable ? 'Authentication required' : 'Tailscale authorization required');
  }
  const value = await response.json().catch(() => ({}));
  if (!response.ok) throw new Error(value.error || `Request failed (${response.status})`);
  $('auth').hidden = true;
  return value;
}

const text = (value) => document.createTextNode(value == null ? '—' : String(value));
function node(tag, className, value) {
  const element = document.createElement(tag);
  if (className) element.className = className;
  if (value !== undefined) element.append(text(value));
  return element;
}
function bytes(value) {
  if (value == null) return 'unknown';
  const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB']; let unit = 0; let number = value;
  while (number >= 1024 && unit < units.length - 1) { number /= 1024; unit += 1; }
  return `${number.toFixed(unit ? 1 : 0)} ${units[unit]}`;
}
function dateTime(value) {
  return value == null ? 'not started' : new Date(value * 1000).toLocaleString();
}
function duration(seconds) {
  seconds = Math.max(0, Math.floor(seconds));
  const days = Math.floor(seconds / 86400); seconds %= 86400;
  const hours = Math.floor(seconds / 3600); seconds %= 3600;
  const minutes = Math.floor(seconds / 60); seconds %= 60;
  return [days && `${days}d`, (days || hours) && `${hours}h`, (days || hours || minutes) && `${minutes}m`, `${seconds}s`].filter(Boolean).join(' ');
}

function renderOverview(status) {
  const capacity = `${bytes(status.archive_filesystem_available_bytes)} free of ${bytes(status.archive_filesystem_total_bytes)} (${status.archive_filesystem_available_percent}%)`;
  const storage = status.archive_filesystem_low_space ? `Low space · ${capacity}` : capacity;
  const repositories = `${status.repository_count} · ${status.model_repository_count} models · ${status.dataset_repository_count} datasets`;
  const values = [['Service', status.ready ? 'Ready' : 'Not ready'], ['Repositories', repositories], ['Archive size', bytes(status.logical_archive_bytes)], ['Storage', storage], ['Measured path', status.archive_filesystem_path], ['Pull-through', status.pullthrough_enabled ? 'Enabled' : 'Disabled']];
  $('overview').replaceChildren(...values.map(([label, value]) => { const card = node('article', 'metric'); card.append(node('span', '', label), node('strong', '', value)); return card; }));
}

function renderRepositories(page) {
  if (!page.items.length) { $('repositories').replaceChildren(node('p', 'empty', 'No archived repositories.')); return; }
  $('repositories').replaceChildren(...page.items.map((repo) => {
    const button = node('button', 'list-row'); button.type = 'button';
    const title = node('span'); const heading = node('strong'); heading.append(node('span', `badge ${repo.repo_type}`, repo.repo_type), text(` ${repo.repo_id}`)); title.append(heading, node('small', '', `${repo.revision_count} revisions · ${bytes(repo.logical_bytes)}`));
    button.append(title, node('span', 'arrow', '→'));
    button.addEventListener('click', () => loadRepository(repo.repo_type, repo.repo_id)); return button;
  }));
}

async function loadRepository(repoType, repoId) {
  try {
    const value = await api(`/api/admin/v1/repositories/${encodeURIComponent(repoType)}/${repoId.split('/').map(encodeURIComponent).join('/')}`);
    $('detail-title').textContent = `${repoType}: ${repoId}`; $('repo-type').value = repoType; $('repo-id').value = repoId;
    const refs = node('div'); refs.append(node('h3', '', 'Refs'));
    const refList = node('ul', 'compact'); Object.entries(value.refs || {}).forEach(([name, commit]) => { const li = node('li'); li.append(node('code', '', name), text(' → '), node('code', '', commit)); refList.append(li); }); refs.append(refList);
    const revisions = node('div'); revisions.append(node('h3', '', 'Revisions'));
    const revisionList = node('ul', 'compact'); (value.revisions || []).forEach((revision) => { const li = node('li'); li.append(node('code', '', revision.commit), text(` · ${revision.file_count} files · ${bytes(revision.logical_bytes)}`)); revisionList.append(li); }); revisions.append(revisionList);
    $('repository-detail').replaceChildren(refs, revisions);
  } catch (error) { $('repository-detail').replaceChildren(node('p', 'error', error.message)); }
}

function cancelButton(label, run) {
  const button = node('button', 'secondary cancel', label); button.type = 'button';
  button.addEventListener('click', async () => {
    button.disabled = true;
    try { const answer = await run(); $('form-message').textContent = `Cancellation: ${answer.cancellation.replace(/_/g, ' ')}.`; await load(); }
    catch (error) { $('form-message').textContent = error.message; }
    finally { button.disabled = false; }
  });
  return button;
}

// A cancellation is a state-changing request with no body, so it carries the
// CSRF header explicitly rather than relying on the body-implies-write default.
const cancel = (path) => api(path, {method: 'DELETE', headers: {'X-ModelKeep-CSRF': '1'}});

function renderAcquisitions(view) {
  $('slots').textContent = `${view.transferring}/${view.transfer_limit} transferring · ${view.waiting} waiting`;
  if (!view.items.length) { $('acquisitions').replaceChildren(node('p', 'empty', 'No upstream acquisition is in flight.')); return; }
  $('acquisitions').replaceChildren(...view.items.map((item) => {
    const row = node('article', 'job'); const top = node('div', 'job-top');
    top.append(node('strong', '', `${item.repo_type}: ${item.repo_id}@${item.requested_revision}`), node('span', `badge ${item.state}`, item.state.replace(/_/g, ' ')));
    row.append(top);
    const selection = [...(item.include || []).map((pattern) => `+${pattern}`), ...(item.exclude || []).map((pattern) => `-${pattern}`)];
    row.append(node('small', 'job-meta', selection.length ? `selection ${selection.join(' ')}` : 'whole repository'));
    const parts = [`${item.operation.replace(/_/g, ' ')}`, item.phase];
    parts.push(item.total_bytes == null ? `${bytes(item.transferred_bytes)} transferred` : `${bytes(item.transferred_bytes)} / ${bytes(item.total_bytes)}`);
    if (item.cancelled) parts.push('cancelling');
    row.append(node('small', 'job-meta', parts.join(' · ')));
    top.append(cancelButton('Cancel acquisition', () => cancel(`/api/admin/v1/acquisitions/${encodeURIComponent(item.id)}`)));
    return row;
  }));
}

function renderJobs(page, append = false) {
  if (!page.items.length && !append) { $('jobs').replaceChildren(node('p', 'empty', 'No management jobs yet.')); }
  const rows = page.items.map((job) => {
    const row = node('article', 'job'); const top = node('div', 'job-top');
    const active = job.state === 'queued' || job.state === 'running';
    top.append(node('strong', '', job.kind), node('span', `badge ${job.state}`, job.state)); row.append(top);
    row.append(node('p', 'job-target', job.repo_id ? `${job.repo_type}: ${job.repo_id}@${job.revision}` : 'entire archive'));
  if (job.kind === 'prefetch') { const selection = [...(job.include || []).map((pattern) => `+${pattern}`), ...(job.exclude || []).map((pattern) => `-${pattern}`)]; row.append(node('small', 'job-meta', selection.length ? `selection ${selection.join(' ')}` : 'whole repository')); }
  if (job.outcome) { row.append(node('small', 'job-meta', `outcome ${job.outcome.replace(/_/g, ' ')}`)); }
    if (job.principal) row.append(node('small', 'job-meta', `Started by ${job.principal.login || job.principal.auth_method}`));
    if (job.started_at != null) {
      const end = job.finished_at == null ? Date.now() / 1000 : job.finished_at;
      row.append(node('small', 'job-meta', `Started ${dateTime(job.started_at)} · elapsed ${duration(end - job.started_at)}`));
    } else {
      row.append(node('small', 'job-meta', `Queued ${dateTime(job.created_at)} · not started`));
    }
    const parts = [job.phase];
    if (job.resumed) parts.push('resumed partial download');
    if (job.total_bytes == null) parts.push(`${bytes(job.progress_bytes)} · total unknown`); else parts.push(`${bytes(job.progress_bytes || 0)} / ${bytes(job.total_bytes)}`);
    if (job.progress_files != null) parts.push(job.total_files == null ? `${job.progress_files} files` : `${job.progress_files} / ${job.total_files} files`);
    if (job.state === 'running' && job.progress_bytes != null) {
      const now = Date.now() / 1000; const previous = progressSamples.get(job.id);
      if (previous && job.progress_bytes >= previous.bytes && now > previous.at) parts.push(`${bytes((job.progress_bytes - previous.bytes) / (now - previous.at))}/s`);
      progressSamples.set(job.id, {bytes: job.progress_bytes, at: now});
    }
    if (job.state === 'running' && job.last_progress_at) {
      const idle = Math.max(0, Math.floor(Date.now() / 1000) - job.last_progress_at); parts.push(`${idle}s since progress`);
      if (idle >= 120) row.classList.add('stalled');
    }
    row.append(node('small', 'job-meta', parts.join(' · ')));
    if (active && job.total_bytes > 0) { const bar = node('progress', 'job-progress'); bar.max = job.total_bytes; bar.value = Math.min(job.progress_bytes || 0, job.total_bytes); row.append(bar); }
    else if (active) { row.append(node('progress', 'job-progress')); }
    if (job.message) row.append(node('p', 'error', `${job.error_class}: ${job.message}`));
    if (active) top.append(cancelButton('Cancel job', () => cancel(`/api/admin/v1/jobs/${encodeURIComponent(job.id)}`)));
    return row;
  });
  if (append) $('jobs').append(...rows); else if (rows.length) $('jobs').replaceChildren(...rows);
  jobsCursor = page.next_cursor || null; $('more-jobs').hidden = jobsCursor == null;
}

async function load() {
  try {
    const [status, repositories, jobs, acquisitions] = await Promise.all([api('/api/admin/v1/status'), api('/api/admin/v1/repositories?limit=50'), api('/api/admin/v1/jobs?limit=50'), api('/api/admin/v1/acquisitions')]);
    renderOverview(status); renderRepositories(repositories); if (!jobsExpanded) renderJobs(jobs); renderAcquisitions(acquisitions);
    const identity = status.principal.name || status.principal.login || status.principal.auth_method;
    $('connection').textContent = `Connected as ${identity} · v${status.version}`;
  } catch (error) { if (!error.message.includes('authorization required') && error.message !== 'Authentication required') $('connection').textContent = error.message; }
}

$('auth-form').addEventListener('submit', (event) => { event.preventDefault(); token = $('token').value; sessionStorage.setItem('modelkeep-admin-token', token); load(); });
$('refresh').addEventListener('click', () => { jobsExpanded = false; load(); });
$('more-jobs').addEventListener('click', async () => {
  if (!jobsCursor) return;
  const button = $('more-jobs'); button.disabled = true;
  try { jobsExpanded = true; renderJobs(await api(`/api/admin/v1/jobs?limit=50&cursor=${encodeURIComponent(jobsCursor)}`), true); }
  catch (error) { $('connection').textContent = error.message; }
  finally { button.disabled = false; }
});
$('kind').addEventListener('change', () => { const kind = $('kind').value; const audit = kind === 'audit'; $('target-fields').hidden = audit; $('repo-id').required = !audit; $('revision').required = !audit; $('selection-fields').hidden = kind !== 'prefetch'; });
$('job-form').addEventListener('submit', async (event) => {
  event.preventDefault(); const kind = $('kind').value; const body = {kind};
  if (kind !== 'audit') { body.repo_type = $('repo-type').value; body.repo_id = $('repo-id').value.trim(); body.revision = $('revision').value.trim(); }
  if (kind === 'prefetch') { const patterns = (id) => $(id).value.split('\n').map((line) => line.trim()).filter((line) => line !== ''); const include = patterns('include'); const exclude = patterns('exclude'); if (include.length) body.include = include; if (exclude.length) body.exclude = exclude; }
  $('submit-job').disabled = true; $('form-message').textContent = 'Submitting…';
  try { const job = await api('/api/admin/v1/jobs', {method: 'POST', body: JSON.stringify(body)}); $('repo-id').value = ''; $('form-message').textContent = `Job ${job.id} queued.`; $('include').value = ''; $('exclude').value = ''; await load(); }
  catch (error) { $('form-message').textContent = error.message; }
  finally { $('submit-job').disabled = false; }
});
load(); timer = setInterval(load, 3000); window.addEventListener('pagehide', () => clearInterval(timer));
"#;

const STYLE: &str = r#":root{color-scheme:dark;--bg:#0b1014;--panel:#121a20;--line:#26343d;--text:#edf5f2;--muted:#91a29f;--accent:#71e0b1;--warn:#ffd166;--error:#ff8e8e;font:16px/1.5 system-ui,sans-serif}*{box-sizing:border-box}body{margin:0;background:radial-gradient(circle at top left,#15352c 0,transparent 30rem),var(--bg);color:var(--text)}header,main{width:min(1180px,calc(100% - 2rem));margin:auto}header{display:flex;justify-content:space-between;align-items:end;padding:3rem 0 2rem;border-bottom:1px solid var(--line)}h1,h2,h3,p{margin-top:0}h1{font-size:clamp(2.5rem,8vw,5rem);line-height:.9;margin-bottom:0}h2{font-size:1.35rem;margin-bottom:1rem}.eyebrow{text-transform:uppercase;letter-spacing:.14em;color:var(--accent);font-size:.72rem;font-weight:700;margin-bottom:.5rem}main{display:grid;gap:2rem;padding:2rem 0 5rem}.panel,.metric{background:color-mix(in srgb,var(--panel) 92%,transparent);border:1px solid var(--line);border-radius:14px;padding:1.25rem}.auth{display:flex;justify-content:space-between;gap:2rem;align-items:end}.grid{display:grid;grid-template-columns:1fr 1fr;gap:1rem}.operations{grid-template-columns:minmax(16rem,.7fr) minmax(20rem,1.3fr)}.metrics{display:grid;grid-template-columns:repeat(4,1fr);gap:1rem}.metric span,.metric strong{display:block}.metric span,small,.empty,#connection{color:var(--muted)}.metric strong{font-size:1.3rem;margin-top:.4rem}.section-heading,.job-top,.inline{display:flex;align-items:center;justify-content:space-between;gap:1rem}.section-heading h2{margin-bottom:0}.list{display:grid;gap:.55rem}.list-row,.job{width:100%;text-align:left;background:#0d1519;border:1px solid var(--line);border-radius:10px;padding:.85rem;color:inherit}.list-row{display:flex;align-items:center;justify-content:space-between;cursor:pointer}.list-row:hover,.list-row:focus-visible{border-color:var(--accent)}.list-row span:first-child,.list-row small,.job-meta{display:block}.arrow{color:var(--accent)}button,input,select,textarea{font:inherit;border-radius:8px;border:1px solid var(--line);padding:.68rem .8rem}button{background:var(--accent);color:#082018;border:0;font-weight:750;cursor:pointer}button.secondary{background:transparent;color:var(--text);border:1px solid var(--line)}button:disabled{opacity:.55}input,select,textarea{width:100%;background:#0b1115;color:var(--text);margin:.3rem 0 1rem}label{display:block;font-weight:650}.auth form{min-width:min(26rem,100%)}.inline input{margin:0}.badge{padding:.15rem .55rem;border-radius:99px;background:#26343d;font-size:.75rem}.badge.dataset{color:#9fc5ff}.badge.completed{color:var(--accent)}.badge.failed{color:var(--error)}.badge.running,.badge.waiting_for_transfer_slot{color:var(--warn)}.badge.cancelled{color:var(--muted)}.badge.transferring{color:var(--accent)}button.cancel{padding:.35rem .7rem;font-weight:650}.job p{margin:.35rem 0}.error{color:var(--error);overflow-wrap:anywhere}.compact{padding-left:1.2rem}.compact li{margin:.45rem 0;overflow-wrap:anywhere}code{font-size:.82rem}.detail{display:grid;gap:1rem}@media(max-width:760px){header{align-items:start;gap:1rem}.grid,.operations,.metrics{grid-template-columns:1fr}.auth{display:block}.section-heading{align-items:end}.jobs-panel{min-width:0}}"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ui_assets_have_strict_browser_security_headers() {
        let response = index().await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        assert!(response.headers()[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .contains("frame-ancestors 'none'"));
        assert!(!SCRIPT.contains("localStorage"));
        assert!(SCRIPT.contains("X-ModelKeep-CSRF"));
        assert!(INDEX.contains("class=\"panel authentication\" hidden"));
        assert!(SCRIPT.contains("x-modelkeep-auth-methods"));
        assert!(SCRIPT.contains("Connected as"));
        assert!(SCRIPT.contains("function dateTime(value)"));
        assert!(SCRIPT.contains("elapsed ${duration(end - job.started_at)}"));
        assert!(SCRIPT.contains("const active = job.state === 'queued' || job.state === 'running'"));
        assert!(SCRIPT.contains("job.state === 'running' && job.last_progress_at"));
        assert!(SCRIPT.contains("job.state === 'running' && job.progress_bytes != null"));
        assert!(SCRIPT.contains("if (active && job.total_bytes > 0)"));
        assert!(SCRIPT.contains("node('small', 'job-meta'"));
        assert!(STYLE.contains(".job-meta{display:block}"));
        assert!(SCRIPT.contains("const job = await api('/api/admin/v1/jobs'"));
        assert!(INDEX.contains("id=\"more-jobs\""));
        assert!(SCRIPT.contains("cursor=${encodeURIComponent(jobsCursor)}"));
        assert!(SCRIPT.contains("archive_filesystem_available_bytes"));
        assert!(SCRIPT.contains("archive_filesystem_low_space ? `Low space"));
        assert!(SCRIPT.contains("archive_filesystem_path"));
        assert!(SCRIPT.contains("$('repo-id').value = ''; $('form-message').textContent"));
        assert!(INDEX.contains("id=\"repo-type\""));
        assert!(INDEX.contains("<option value=\"dataset\">Dataset</option>"));
        assert!(SCRIPT.contains("body.repo_type = $('repo-type').value"));
        assert!(SCRIPT.contains("repositories/${encodeURIComponent(repoType)}/"));
        assert!(SCRIPT.contains("`badge ${repo.repo_type}`"));
        assert!(SCRIPT.contains("`${job.repo_type}: ${job.repo_id}@${job.revision}`"));
        assert!(SCRIPT.contains("status.dataset_repository_count"));
        assert!(STYLE.contains(".badge.dataset"));
        assert!(INDEX.contains("id=\"selection-fields\""));
        assert!(INDEX.contains("id=\"include\""));
        assert!(INDEX.contains("id=\"exclude\""));
        assert!(SCRIPT.contains("'selection-fields').hidden = kind !== 'prefetch'"));
        assert!(SCRIPT.contains("if (include.length) body.include = include"));
        assert!(SCRIPT.contains("if (exclude.length) body.exclude = exclude"));
        assert!(SCRIPT.contains(
            "selection.length ? `selection ${selection.join(' ')}` : 'whole repository'"
        ));
        assert!(SCRIPT.contains("queued.`; $('include').value = ''; $('exclude').value = ''"));
        assert!(SCRIPT.contains("`outcome ${job.outcome.replace(/_/g, ' ')}`"));
        assert!(STYLE.contains("input,select,textarea{width:100%"));
        // Issue 0076: a running job and an in-flight acquisition are both
        // cancellable from the UI, and cancellation carries CSRF explicitly
        // because it sends no body.
        assert!(INDEX.contains("id=\"acquisitions\""));
        assert!(INDEX.contains("id=\"slots\""));
        assert!(SCRIPT.contains("method: 'DELETE', headers: {'X-ModelKeep-CSRF': '1'}"));
        assert!(SCRIPT.contains("cancelButton('Cancel job'"));
        assert!(SCRIPT.contains("cancelButton('Cancel acquisition'"));
        assert!(SCRIPT.contains("/api/admin/v1/jobs/${encodeURIComponent(job.id)}"));
        assert!(SCRIPT.contains("/api/admin/v1/acquisitions/${encodeURIComponent(item.id)}"));
        assert!(SCRIPT.contains("api('/api/admin/v1/acquisitions')"));
        assert!(SCRIPT.contains("transferring · ${view.waiting} waiting"));
        assert!(SCRIPT.contains("if (item.cancelled) parts.push('cancelling')"));
        assert!(STYLE.contains(".badge.waiting_for_transfer_slot"));
    }
}
