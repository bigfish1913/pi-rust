const $ = (selector, scope = document) => scope.querySelector(selector);
const $$ = (selector, scope = document) => [...scope.querySelectorAll(selector)];
const state = { data: null, mode: 'guide', query: '' };

function showToast(message) {
  const toast = $('[data-toast]'); toast.textContent = message; toast.classList.add('is-visible');
  clearTimeout(showToast.timer); showToast.timer = setTimeout(() => toast.classList.remove('is-visible'), 2200);
}

function bindMenu() {
  const toggle = $('[data-menu-toggle]'); const menu = $('[data-mobile-menu]');
  toggle.addEventListener('click', () => { const open = menu.classList.toggle('is-open'); toggle.setAttribute('aria-expanded', String(open)); });
  $$('a', menu).forEach((link) => link.addEventListener('click', () => { menu.classList.remove('is-open'); toggle.setAttribute('aria-expanded', 'false'); }));
}

function bindScrollEffects() {
  const header = $('[data-sticky-header]'); const marker = document.createElement('div'); marker.className = 'scroll-marker'; document.body.prepend(marker);
  new IntersectionObserver(([entry]) => header.classList.toggle('is-scrolled', !entry.isIntersecting), { threshold: 0 }).observe(marker);
}

function currentMode() { return state.data.modes[state.mode]; }

function matches(section) {
  const query = state.query.trim().toLowerCase();
  if (!query) return true;
  return `${section.title} ${section.summary} ${section.kicker} ${section.body.join(' ')} ${section.code?.file || ''}`.toLowerCase().includes(query);
}

function renderModes() {
  $('[data-docs-modes]').innerHTML = Object.entries(state.data.modes).map(([key, mode], index) => `<button class="docs-mode ${key === state.mode ? 'is-active' : ''}" type="button" role="tab" aria-selected="${key === state.mode}" data-docs-mode="${key}"><span>0${index + 1}</span>${mode.label}</button>`).join('');
  $$('[data-docs-mode]').forEach((button) => button.addEventListener('click', () => { state.mode = button.dataset.docsMode; state.query = ''; $('[data-docs-search]').value = ''; render(); }));
}

function renderNav(sections) {
  const nav = $('[data-docs-nav]');
  nav.innerHTML = sections.length ? sections.map((section) => `<a href="#${section.id}"><span>${section.kicker.split(' / ')[0]}</span>${section.title}</a>`).join('') : '';
  $$('a', nav).forEach((link) => link.addEventListener('click', (event) => { event.preventDefault(); document.getElementById(link.hash.slice(1))?.scrollIntoView({ behavior: 'smooth', block: 'start' }); history.replaceState(null, '', link.hash); }));
}

function sectionMarkup(section) {
  const links = (section.links || []).map((link) => `<a href="${link.url}" target="_blank" rel="noreferrer">${link.label} ↗</a>`).join('');
  const code = section.code ? `<div class="docs-code"><div class="docs-code-top"><span>${section.code.file}</span><button type="button" data-copy-docs-code aria-label="复制代码">复制 <span aria-hidden="true">⧉</span></button></div><pre><code>${section.code.content}</code></pre></div>` : '';
  return `<article class="docs-section" id="${section.id}"><div class="docs-section-heading"><span class="eyebrow eyebrow-dark">${section.kicker}</span><h2>${section.title}</h2><p class="docs-summary">${section.summary}</p></div><div class="docs-section-body">${section.body.map((paragraph) => `<p>${paragraph}</p>`).join('')}${code}<div class="docs-links">${links}</div></div></article>`;
}

function bindCodeCopy() {
  $$('[data-copy-docs-code]').forEach((button) => button.addEventListener('click', async () => {
    try { await navigator.clipboard.writeText(button.closest('.docs-code').querySelector('code').textContent); showToast('代码已复制'); } catch { showToast('复制失败，请手动选择文本'); }
  }));
}

function render() {
  const mode = currentMode(); const sections = mode.sections.filter(matches);
  $('[data-docs-eyebrow]').textContent = mode.eyebrow; $('[data-docs-description]').textContent = mode.description;
  renderModes(); renderNav(sections); $('[data-docs-sections]').innerHTML = sections.map(sectionMarkup).join(''); $('[data-docs-empty]').hidden = sections.length > 0; bindCodeCopy();
}

async function init() {
  const response = await fetch('./data/docs.json?v=2'); state.data = await response.json();
  bindMenu(); bindScrollEffects(); render();
  $('[data-docs-search]').addEventListener('input', (event) => { state.query = event.target.value; render(); });
}

init().catch((error) => { console.error(error); showToast('文档数据加载失败，请通过本地开发服务器打开网站'); });
