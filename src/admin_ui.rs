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
          <div id="target-fields"><label for="repo-id">Repository</label><input id="repo-id" placeholder="namespace/model" required><label for="revision">Revision or ref</label><input id="revision" value="main" required></div>
          <button id="submit-job">Start job</button>
        </form>
        <p id="form-message" role="status"></p>
      </div>
      <div class="panel jobs-panel">
        <div class="section-heading"><div><p class="eyebrow">Persistent operations</p><h2>Recent jobs</h2></div><span>latest 50</span></div>
        <div id="jobs" class="list" aria-live="polite"><p class="empty">No jobs loaded.</p></div>
      </div>
    </section>
  </main>
  <script src="/admin/app.js" defer></script>
</body>
</html>"#;

const SCRIPT: &str = r#"'use strict';
const $ = (id) => document.getElementById(id);
let token = sessionStorage.getItem('modelkeep-admin-token') || '';
let timer;
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

function renderOverview(status) {
  const values = [['Service', status.ready ? 'Ready' : 'Not ready'], ['Repositories', status.repository_count], ['Archive size', bytes(status.logical_archive_bytes)], ['Pull-through', status.pullthrough_enabled ? 'Enabled' : 'Disabled']];
  $('overview').replaceChildren(...values.map(([label, value]) => { const card = node('article', 'metric'); card.append(node('span', '', label), node('strong', '', value)); return card; }));
}

function renderRepositories(page) {
  if (!page.items.length) { $('repositories').replaceChildren(node('p', 'empty', 'No archived repositories.')); return; }
  $('repositories').replaceChildren(...page.items.map((repo) => {
    const button = node('button', 'list-row'); button.type = 'button';
    const title = node('span'); title.append(node('strong', '', repo.repo_id), node('small', '', `${repo.revision_count} revisions · ${bytes(repo.logical_bytes)}`));
    button.append(title, node('span', 'arrow', '→'));
    button.addEventListener('click', () => loadRepository(repo.repo_id)); return button;
  }));
}

async function loadRepository(repoId) {
  try {
    const value = await api(`/api/admin/v1/repositories/${repoId.split('/').map(encodeURIComponent).join('/')}`);
    $('detail-title').textContent = repoId; $('repo-id').value = repoId;
    const refs = node('div'); refs.append(node('h3', '', 'Refs'));
    const refList = node('ul', 'compact'); Object.entries(value.refs || {}).forEach(([name, commit]) => { const li = node('li'); li.append(node('code', '', name), text(' → '), node('code', '', commit)); refList.append(li); }); refs.append(refList);
    const revisions = node('div'); revisions.append(node('h3', '', 'Revisions'));
    const revisionList = node('ul', 'compact'); (value.revisions || []).forEach((revision) => { const li = node('li'); li.append(node('code', '', revision.commit), text(` · ${revision.file_count} files · ${bytes(revision.logical_bytes)}`)); revisionList.append(li); }); revisions.append(revisionList);
    $('repository-detail').replaceChildren(refs, revisions);
  } catch (error) { $('repository-detail').replaceChildren(node('p', 'error', error.message)); }
}

function renderJobs(page) {
  if (!page.items.length) { $('jobs').replaceChildren(node('p', 'empty', 'No management jobs yet.')); return; }
  $('jobs').replaceChildren(...page.items.map((job) => {
    const row = node('article', 'job'); const top = node('div', 'job-top');
    top.append(node('strong', '', job.kind), node('span', `badge ${job.state}`, job.state)); row.append(top);
    row.append(node('p', 'job-target', job.repo_id ? `${job.repo_id}@${job.revision}` : 'entire archive'));
    if (job.principal) row.append(node('small', '', `Started by ${job.principal.login || job.principal.auth_method}`));
    const parts = [job.phase];
    if (job.total_bytes == null) parts.push(`${bytes(job.progress_bytes)} · total unknown`); else parts.push(`${bytes(job.progress_bytes || 0)} / ${bytes(job.total_bytes)}`);
    if (job.progress_files != null) parts.push(job.total_files == null ? `${job.progress_files} files` : `${job.progress_files} / ${job.total_files} files`);
    if (job.progress_bytes != null) {
      const now = Date.now() / 1000; const previous = progressSamples.get(job.id);
      if (previous && job.progress_bytes >= previous.bytes && now > previous.at) parts.push(`${bytes((job.progress_bytes - previous.bytes) / (now - previous.at))}/s`);
      progressSamples.set(job.id, {bytes: job.progress_bytes, at: now});
    }
    if (job.last_progress_at) {
      const idle = Math.max(0, Math.floor(Date.now() / 1000) - job.last_progress_at); parts.push(`${idle}s since progress`);
      if (job.state === 'running' && idle >= 120) row.classList.add('stalled');
    }
    row.append(node('small', '', parts.join(' · ')));
    if (job.total_bytes > 0) { const bar = node('progress', 'job-progress'); bar.max = job.total_bytes; bar.value = Math.min(job.progress_bytes || 0, job.total_bytes); row.append(bar); }
    else if (job.state === 'running') { row.append(node('progress', 'job-progress')); }
    if (job.message) row.append(node('p', 'error', `${job.error_class}: ${job.message}`)); return row;
  }));
}

async function load() {
  try {
    const [status, repositories, jobs] = await Promise.all([api('/api/admin/v1/status'), api('/api/admin/v1/repositories?limit=50'), api('/api/admin/v1/jobs?limit=50')]);
    renderOverview(status); renderRepositories(repositories); renderJobs(jobs);
    const identity = status.principal.name || status.principal.login || status.principal.auth_method;
    $('connection').textContent = `Connected as ${identity} · v${status.version}`;
  } catch (error) { if (!error.message.includes('authorization required') && error.message !== 'Authentication required') $('connection').textContent = error.message; }
}

$('auth-form').addEventListener('submit', (event) => { event.preventDefault(); token = $('token').value; sessionStorage.setItem('modelkeep-admin-token', token); load(); });
$('refresh').addEventListener('click', load);
$('kind').addEventListener('change', () => { const audit = $('kind').value === 'audit'; $('target-fields').hidden = audit; $('repo-id').required = !audit; $('revision').required = !audit; });
$('job-form').addEventListener('submit', async (event) => {
  event.preventDefault(); const kind = $('kind').value; const body = {kind};
  if (kind !== 'audit') { body.repo_id = $('repo-id').value.trim(); body.revision = $('revision').value.trim(); }
  $('submit-job').disabled = true; $('form-message').textContent = 'Submitting…';
  try { const job = await api('/api/admin/v1/jobs', {method: 'POST', body: JSON.stringify(body)}); $('repo-id').value = ''; $('form-message').textContent = `Job ${job.id} queued.`; await load(); }
  catch (error) { $('form-message').textContent = error.message; }
  finally { $('submit-job').disabled = false; }
});
load(); timer = setInterval(load, 3000); window.addEventListener('pagehide', () => clearInterval(timer));
"#;

const STYLE: &str = r#":root{color-scheme:dark;--bg:#0b1014;--panel:#121a20;--line:#26343d;--text:#edf5f2;--muted:#91a29f;--accent:#71e0b1;--warn:#ffd166;--error:#ff8e8e;font:16px/1.5 system-ui,sans-serif}*{box-sizing:border-box}body{margin:0;background:radial-gradient(circle at top left,#15352c 0,transparent 30rem),var(--bg);color:var(--text)}header,main{width:min(1180px,calc(100% - 2rem));margin:auto}header{display:flex;justify-content:space-between;align-items:end;padding:3rem 0 2rem;border-bottom:1px solid var(--line)}h1,h2,h3,p{margin-top:0}h1{font-size:clamp(2.5rem,8vw,5rem);line-height:.9;margin-bottom:0}h2{font-size:1.35rem;margin-bottom:1rem}.eyebrow{text-transform:uppercase;letter-spacing:.14em;color:var(--accent);font-size:.72rem;font-weight:700;margin-bottom:.5rem}main{display:grid;gap:2rem;padding:2rem 0 5rem}.panel,.metric{background:color-mix(in srgb,var(--panel) 92%,transparent);border:1px solid var(--line);border-radius:14px;padding:1.25rem}.auth{display:flex;justify-content:space-between;gap:2rem;align-items:end}.grid{display:grid;grid-template-columns:1fr 1fr;gap:1rem}.operations{grid-template-columns:minmax(16rem,.7fr) minmax(20rem,1.3fr)}.metrics{display:grid;grid-template-columns:repeat(4,1fr);gap:1rem}.metric span,.metric strong{display:block}.metric span,small,.empty,#connection{color:var(--muted)}.metric strong{font-size:1.3rem;margin-top:.4rem}.section-heading,.job-top,.inline{display:flex;align-items:center;justify-content:space-between;gap:1rem}.section-heading h2{margin-bottom:0}.list{display:grid;gap:.55rem}.list-row,.job{width:100%;text-align:left;background:#0d1519;border:1px solid var(--line);border-radius:10px;padding:.85rem;color:inherit}.list-row{display:flex;align-items:center;justify-content:space-between;cursor:pointer}.list-row:hover,.list-row:focus-visible{border-color:var(--accent)}.list-row span:first-child,.list-row small{display:block}.arrow{color:var(--accent)}button,input,select{font:inherit;border-radius:8px;border:1px solid var(--line);padding:.68rem .8rem}button{background:var(--accent);color:#082018;border:0;font-weight:750;cursor:pointer}button.secondary{background:transparent;color:var(--text);border:1px solid var(--line)}button:disabled{opacity:.55}input,select{width:100%;background:#0b1115;color:var(--text);margin:.3rem 0 1rem}label{display:block;font-weight:650}.auth form{min-width:min(26rem,100%)}.inline input{margin:0}.badge{padding:.15rem .55rem;border-radius:99px;background:#26343d;font-size:.75rem}.badge.completed{color:var(--accent)}.badge.failed{color:var(--error)}.badge.running{color:var(--warn)}.job p{margin:.35rem 0}.error{color:var(--error);overflow-wrap:anywhere}.compact{padding-left:1.2rem}.compact li{margin:.45rem 0;overflow-wrap:anywhere}code{font-size:.82rem}.detail{display:grid;gap:1rem}@media(max-width:760px){header{align-items:start;gap:1rem}.grid,.operations,.metrics{grid-template-columns:1fr}.auth{display:block}.section-heading{align-items:end}.jobs-panel{min-width:0}}"#;

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
        assert!(SCRIPT.contains("const job = await api('/api/admin/v1/jobs'"));
        assert!(SCRIPT.contains("$('repo-id').value = ''; $('form-message').textContent"));
    }
}
