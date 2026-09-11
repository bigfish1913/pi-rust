import { getLocale, initI18n, localizeData, onLocaleChange, t } from './i18n.js';

const state = { rawData: null, data: null, activeExample: 0 };
const $ = (selector, scope = document) => scope.querySelector(selector);
const $$ = (selector, scope = document) => [...scope.querySelectorAll(selector)];

function getPath(source, path) { return path.split('.').reduce((value, key) => value?.[key], source); }

function renderFeatures(features) {
  $('[data-features]').innerHTML = features.map((feature) => `
    <article class="feature-card"><div class="feature-top"><span class="feature-number">${feature.number}</span><span class="feature-tag">${feature.tag}</span></div><h3>${feature.title}</h3><p>${feature.description}</p><span class="feature-line"></span></article>
  `).join('');
}

function renderArchitecture(layers) {
  $('[data-architecture]').innerHTML = layers.map((layer, index) => `
    <div class="architecture-layer tone-${layer.tone}"><div class="layer-index">0${index + 1}</div><div class="layer-name">${layer.name}</div><div class="layer-label">${layer.label}</div><p>${layer.description}</p>${index < layers.length - 1 ? '<span class="layer-arrow" aria-hidden="true">↓</span>' : ''}</div>
  `).join('');
}

function renderExamples(examples) {
  const tabs = $('[data-example-tabs]');
  tabs.innerHTML = examples.map((example, index) => `
    <button class="code-tab ${index === state.activeExample ? 'is-active' : ''}" type="button" role="tab" aria-selected="${index === state.activeExample}" data-example-index="${index}"><span class="tab-dot"></span>${example.label}</button>
  `).join('');
  $$('.code-tab', tabs).forEach((tab) => tab.addEventListener('click', () => { state.activeExample = Number(tab.dataset.exampleIndex); updateExample(examples); }));
  updateExample(examples);
}

function updateExample(examples) {
  const example = examples[state.activeExample];
  $$('.code-tab').forEach((tab, index) => { const active = index === state.activeExample; tab.classList.toggle('is-active', active); tab.setAttribute('aria-selected', String(active)); });
  $('[data-code-file]').textContent = example.file;
  $('[data-code-note]').textContent = example.note;
  $('[data-code-content]').textContent = example.code;
}

function renderReleases(releases) {
  $('[data-releases]').innerHTML = releases.map((release) => `
    <article class="release-card release-${release.status}"><div class="release-top"><span class="release-version">${release.version}</span><span class="release-status">${release.status}</span></div><h3>${release.title}</h3><p>${release.description}</p><span class="release-mark" aria-hidden="true">↗</span></article>
  `).join('');
}

function bindCopy(buttonSelector, getText, successMessage) {
  $(buttonSelector)?.addEventListener('click', async () => {
    try { await navigator.clipboard.writeText(getText()); showToast(successMessage); } catch { showToast(t('copy.failed')); }
  });
}

function showToast(message) {
  const toast = $('[data-toast]'); toast.textContent = message; toast.classList.add('is-visible');
  window.clearTimeout(showToast.timer); showToast.timer = window.setTimeout(() => toast.classList.remove('is-visible'), 2200);
}

function bindMenu() {
  const toggle = $('[data-menu-toggle]'); const menu = $('[data-mobile-menu]');
  toggle.addEventListener('click', () => { const open = menu.classList.toggle('is-open'); toggle.setAttribute('aria-expanded', String(open)); });
  $$('a', menu).forEach((link) => link.addEventListener('click', () => { menu.classList.remove('is-open'); toggle.setAttribute('aria-expanded', 'false'); }));
}

function bindScrollEffects() {
  const header = $('[data-sticky-header]'); const marker = document.createElement('div'); marker.className = 'scroll-marker'; document.body.prepend(marker);
  const observer = new IntersectionObserver(([entry]) => header.classList.toggle('is-scrolled', !entry.isIntersecting), { threshold: 0 }); observer.observe(marker);
}

async function init() {
  await initI18n();
  const response = await fetch('./data/site.json'); state.rawData = await response.json();
  state.data = localizeData(state.rawData, 'site');
  $$('[data-content]').forEach((element) => {
    const value = getPath(state.data, element.dataset.content);
    if (!value) return;
    if (element.dataset.content === 'hero.title') element.innerHTML = value.includes('<') ? value : `<span class="hero-title-line">把 Agent 做成</span><br /><em class="hero-title-line">你自己的工具。</em>`;
    else element.textContent = value;
  });
  $$('[data-stat]').forEach((element) => { const value = getPath(state.data, element.dataset.stat); if (value) element.textContent = value; });
  renderFeatures(state.data.features); renderArchitecture(state.data.architecture); renderExamples(state.data.examples); renderReleases(state.data.releases);
  bindCopy('[data-copy-code]', () => state.data.examples[state.activeExample].code, t('copy.codeSuccess'));
  $$('[data-copy-command]').forEach((button) => button.addEventListener('click', async () => {
    try { await navigator.clipboard.writeText(button.previousElementSibling.textContent); showToast(t('copy.commandSuccess')); } catch { showToast(t('copy.failed')); }
  }));
  bindMenu(); bindScrollEffects();
  onLocaleChange(() => {
    state.data = localizeData(state.rawData, 'site');
    $$('[data-content]').forEach((element) => {
      const value = getPath(state.data, element.dataset.content);
      if (element.dataset.content === 'hero.title') element.innerHTML = value.includes('<') ? value : `<span class="hero-title-line">把 Agent 做成</span><br /><em class="hero-title-line">你自己的工具。</em>`;
      else element.textContent = value;
    });
    $$('[data-stat]').forEach((element) => { const value = getPath(state.data, element.dataset.stat); if (value) element.textContent = value; });
    renderFeatures(state.data.features); renderArchitecture(state.data.architecture); renderExamples(state.data.examples); renderReleases(state.data.releases);
  });
}

init().catch((error) => { console.error(error); showToast(t('error.data')); });
