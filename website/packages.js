const $ = (selector, scope = document) => scope.querySelector(selector);
const $$ = (selector, scope = document) => [...scope.querySelectorAll(selector)];
const state = { packages: [], query: '', type: 'all', sort: 'downloads' };

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

function sortedPackages() {
  const query = state.query.trim().toLowerCase();
  return state.packages.filter((item) => {
    const haystack = `${item.name} ${item.description} ${item.author}`.toLowerCase();
    return (state.type === 'all' || item.type === state.type) && (!query || haystack.includes(query));
  }).sort((a, b) => {
    if (state.sort === 'name') return a.name.localeCompare(b.name);
    if (state.sort === 'recent') return a.age - b.age;
    return b.downloads - a.downloads;
  });
}

function packageCard(item) {
  return `<article class="package-card"><div class="package-card-top"><span class="package-type type-${item.type}">${item.type}</span><span class="package-age">${item.ageLabel}</span></div><h3><a href="${item.repository}" target="_blank" rel="noreferrer">${item.name} <span aria-hidden="true">↗</span></a></h3><p>${item.description}</p><div class="package-card-bottom"><span class="package-author">${item.author}</span><span class="package-downloads">${item.downloadsLabel}/mo</span></div><div class="package-command"><code>${item.install}</code><button type="button" aria-label="复制 ${item.name} 安装命令" data-copy-package="${item.install}">⧉</button></div></article>`;
}

function renderRecent() {
  $('[data-recent-packages]').innerHTML = state.packages.slice(0, 4).map((item) => `<a class="recent-package" href="${item.repository}" target="_blank" rel="noreferrer"><strong>${item.name}</strong><span>${item.description}</span><small>${item.ageLabel}</small></a>`).join('');
}

function renderList() {
  const items = sortedPackages();
  $('[data-package-count]').textContent = state.packages.length;
  $('[data-results-count]').textContent = `${items.length} 个结果`;
  $('[data-package-list]').innerHTML = items.map(packageCard).join('');
  $('[data-empty-state]').hidden = items.length > 0;
  $$('[data-copy-package]').forEach((button) => button.addEventListener('click', async () => {
    try { await navigator.clipboard.writeText(button.dataset.copyPackage); showToast('安装命令已复制'); } catch { showToast('复制失败，请手动选择文本'); }
  }));
}

async function init() {
  const response = await fetch('./data/packages.json'); state.packages = await response.json();
  renderRecent(); renderList(); bindMenu(); bindScrollEffects();
  $('[data-package-search]').addEventListener('input', (event) => { state.query = event.target.value; renderList(); });
  $('[data-package-type]').addEventListener('change', (event) => { state.type = event.target.value; renderList(); });
  $('[data-package-sort]').addEventListener('change', (event) => { state.sort = event.target.value; renderList(); });
  $('[data-reset]').addEventListener('click', () => { state.query = ''; state.type = 'all'; state.sort = 'downloads'; $('[data-package-search]').value = ''; $('[data-package-type]').value = 'all'; $('[data-package-sort]').value = 'downloads'; renderList(); });
  $('[data-copy-command]').addEventListener('click', async () => { try { await navigator.clipboard.writeText($('[data-copy-command]').previousElementSibling.textContent); showToast('安装命令已复制'); } catch { showToast('复制失败，请手动选择文本'); } });
}

init().catch((error) => { console.error(error); showToast('数据加载失败，请通过本地开发服务器打开网站'); });
