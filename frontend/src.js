import { invoke } from '@tauri-apps/api/core';

const $ = (selector) => document.querySelector(selector);
const state = {
  assets: [], rules: [], counts: { photos: 0, usb: 0, queued: 0, candidates: 0 },
  reviewCounts: { pending: 0, later: 0 },
  page: 'cleanup', ruleId: 'blur', source: 'all', tab: 'all', query: '',
  selected: new Set(), focus: null, offset: 0, photoOffset: 0, usbOffset: 0, limit: 60, total: 0, sortAsc: false, inspectorHidden: false,
  photoStatus: 'unknown', usbStatus: 'unknown', photoTotal: 0, usbViewTotal: 0, similarAnchor: null, searchGeneration: 0, scanTimer: null, toastTimer: null,
};
const labels = {
  all: ['所有照片', '图库'], recent: ['最近导入', '图库'], favorites: ['收藏', '图库'],
  search: ['语义搜索', '整理'], cleanup: ['清理建议', '整理'], similar: ['相似照片', '整理'], queue: ['待删除', '整理'],
};
const svgImage = '<svg viewBox="0 0 20 20" fill="none"><rect x="3" y="3.5" width="14" height="13" rx="2" stroke="currentColor" stroke-width="1.2"/><circle cx="7" cy="8" r="1.3" fill="currentColor"/><path d="m4.5 14 3.5-3.3 2.2 2 2-1.7 3.3 3" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round"/></svg>';
const checkSvg = '<svg viewBox="0 0 16 16" fill="none"><path d="m3.3 8.2 3 3L12.8 4.8" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"/></svg>';

function toast(message) {
  const node = $('#toast'); node.textContent = message; node.classList.add('show');
  clearTimeout(state.toastTimer); state.toastTimer = setTimeout(() => node.classList.remove('show'), 2400);
}

function escapeHtml(value = '') {
  return String(value).replace(/[&<>"']/g, char => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[char]);
}

function formatDate(raw) {
  if (!raw) return '拍摄时间未知';
  const value = new Date(raw);
  if (Number.isNaN(value.getTime())) return raw;
  return new Intl.DateTimeFormat('zh-CN', { year: 'numeric', month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit' }).format(value);
}

function assetKey(asset) { return `${asset.source}:${asset.id}`; }
function sourceName(source) { return source === 'usb' ? 'USB iPhone' : '照片图库'; }
function localSelectionId(asset) { return assetKey(asset); }
function selectedAssets() { return state.assets.filter(asset => state.selected.has(localSelectionId(asset))); }
function resetLibraryOffsets() { state.offset = 0; state.photoOffset = 0; state.usbOffset = 0; }

function updateHeader() {
  const dot = $('#statusDot');
  const photoAvailable = state.photoStatus === 'authorized' || state.photoStatus === 'limited';
  const text = state.source === 'usb'
    ? (state.usbStatus === 'connected' ? 'iPhone 已连接' : state.usbStatus === 'indexing' || state.usbStatus === 'searching' ? '正在查找 iPhone' : state.usbStatus === 'error' ? 'iPhone 连接异常' : 'iPhone 未连接')
    : photoAvailable ? `本地图库 · ${state.counts.photos || 0} 张`
      : state.usbStatus === 'connected' ? `iPhone 已连接 · ${state.counts.usb || 0} 张`
      : state.photoStatus === 'denied' || state.photoStatus === 'restricted' ? '需要照片图库权限' : '等待照片图库授权';
  $('#statusText').textContent = text;
  dot.classList.toggle('busy', state.photoStatus === 'not_determined' || state.usbStatus === 'indexing' || state.usbStatus === 'searching');
  $('#libraryName').textContent = state.usbStatus === 'connected' && state.source === 'usb' ? '已连接 iPhone' : '本地照片图库';
  $('#libraryStatus').textContent = state.photoStatus === 'authorized' ? `已授权 · ${state.counts.photos || 0} 张照片`
    : state.photoStatus === 'limited' ? `有限访问 · ${state.counts.photos || 0} 张可见`
      : state.photoStatus === 'denied' || state.photoStatus === 'restricted' ? '请在系统设置中允许访问'
        : state.usbStatus === 'connected' ? 'USB 已连接，可浏览媒体' : '尚未读取图库';
  $('#allCount').textContent = state.source === 'usb' ? state.counts.usb || 0
    : state.source === 'all' ? (state.counts.photos || 0) + (state.counts.usb || 0) : state.counts.photos || 0;
  $('#queueSideCount').textContent = state.counts.queued || 0;
  $('#queueCount').textContent = `${state.counts.queued || 0} →`;
  $('#candidateCount').textContent = state.counts.candidates || 0;
}

function currentRule() { return state.rules.find(rule => rule.id === state.ruleId); }

function sortTitle() {
  if (state.page === 'cleanup') return '匹配度';
  if (state.page === 'similar') return '相似度';
  if (state.page === 'search') return '相关度';
  return '拍摄时间';
}

function updateSortButton() {
  $('#sortFilter').innerHTML = `${sortTitle()} ${state.sortAsc ? '↑' : '↓'} <svg viewBox="0 0 12 12" fill="none"><path d="m3 4.5 3 3 3-3" stroke="currentColor" stroke-width="1.2" stroke-linecap="round" stroke-linejoin="round"/></svg>`;
}

function setSubheading() {
  const rule = currentRule();
  if (state.page === 'cleanup') {
    $('#subheading').innerHTML = rule
      ? `规则「<strong>${escapeHtml(rule.name)}</strong>」· ${state.total || 0} 张候选待复核或稍后查看，匹配分数仅用于排序`
      : '选择一条扫描规则，发现符合描述的照片候选。';
  } else if (state.page === 'queue') {
    $('#subheading').innerHTML = `<strong>${state.counts.queued || 0} 张</strong>等待最终确认；从这里确认后才会调用系统删除。`;
  } else if (state.page === 'similar') {
    $('#subheading').innerHTML = state.similarAnchor
      ? `与 <strong>“${escapeHtml(state.similarAnchor.name)}”</strong> 相似的已索引照片 · ${state.total} 张`
      : '选择一张已索引照片，在详情面板中查找向量相似照片。';
  } else if (state.page === 'all') {
    $('#subheading').innerHTML = `<strong>${state.total || 0} 张</strong>来自当前照片来源`;
  } else if (state.page === 'search') {
    $('#subheading').innerHTML = state.query
      ? `搜索 <strong>“${escapeHtml(state.query)}”</strong> 的已索引照片`
      : '输入场景或画面描述，在已索引照片中搜索';
  } else {
    $('#subheading').innerHTML = `<strong>${state.total || 0} 张</strong>照片`;
  }
}

function decoratePage() {
  const [title, category] = labels[state.page] || labels.cleanup;
  $('#pageTitle').textContent = title;
  $('#crumbCategory').textContent = category;
  $('#crumbSection').textContent = title;
  $('#crumb').textContent = state.page === 'cleanup' ? (currentRule()?.name || '扫描规则') : title;
  $('#addSelected').textContent = state.page === 'queue' ? '移出待删除' : '加入待删除';
  $('#confirmSelected').classList.toggle('is-hidden', state.page !== 'queue');
  setSubheading();
  document.querySelectorAll('.nav-item[data-page]').forEach(button => button.classList.toggle('active', button.dataset.page === state.page));
  document.querySelectorAll('.rule-nav').forEach(button => button.classList.toggle('active-rule', button.dataset.rule === state.ruleId));
  document.querySelectorAll('.tab').forEach(button => button.classList.toggle('active', button.dataset.tab === state.tab));
}

function filterAssets() {
  let rows = [...state.assets];
  if (state.source !== 'all') rows = rows.filter(asset => asset.source === state.source);
  if (state.page === 'cleanup') rows = rows.filter(asset => asset.rule_id === state.ruleId && asset.score !== null && asset.score !== undefined);
  if (state.page === 'queue') rows = rows.filter(asset => asset.queued);
  if (state.page === 'favorites') rows = rows.filter(asset => asset.favorite);
  if (state.page === 'recent') rows = rows.filter(asset => asset.date).sort((a, b) => new Date(b.date) - new Date(a.date)).slice(0, 60);
  if (state.page === 'similar') rows = rows.filter(asset => asset.score !== null && asset.score !== undefined);
  if (state.page !== 'cleanup' && state.tab !== 'all' && state.page !== 'queue') rows = rows.filter(asset => asset.reviewed === state.tab);
  if (state.query && state.page !== 'search') rows = rows.filter(asset => asset.name.toLowerCase().includes(state.query.toLowerCase()));
  rows.sort((a, b) => {
    if ((a.score ?? -1) !== (b.score ?? -1) && ['cleanup', 'similar', 'search'].includes(state.page)) {
      return !state.sortAsc
        ? (b.score ?? -1) - (a.score ?? -1)
        : (a.score ?? -1) - (b.score ?? -1);
    }
    const order = (a.date || '').localeCompare(b.date || '');
    return state.sortAsc ? order : -order;
  });
  return rows;
}

const THUMBNAIL_CACHE_BUDGET_BYTES = 24 * 1024 * 1024;
const thumbnailCache = new Map();
const thumbnailPending = new Map();
const thumbnailQueue = [];
let thumbnailCacheBytes = 0;
let activeThumbnailJobs = 0;
let thumbnailObserver = null;

function placeholderNode(key) {
  const node = document.createElement('div');
  node.className = 'photo-placeholder';
  node.dataset.thumbnailKey = key;
  node.innerHTML = svgImage;
  return node;
}

function releaseRenderedThumbnail(key) {
  for (const image of document.querySelectorAll('img[data-thumbnail-key]')) {
    if (image.dataset.thumbnailKey !== key) continue;
    thumbnailObserver?.unobserve(image);
    const placeholder = placeholderNode(key);
    image.replaceWith(placeholder);
    thumbnailObserver?.observe(placeholder);
  }
}

function rememberThumbnail(key, value) {
  const bytes = value.length * 2;
  if (bytes > THUMBNAIL_CACHE_BUDGET_BYTES) return false;
  const previous = thumbnailCache.get(key);
  if (previous) thumbnailCacheBytes -= previous.length * 2;
  thumbnailCache.delete(key);
  thumbnailCache.set(key, value);
  thumbnailCacheBytes += bytes;
  while (thumbnailCacheBytes > THUMBNAIL_CACHE_BUDGET_BYTES) {
    const oldestKey = thumbnailCache.keys().next().value;
    const oldest = thumbnailCache.get(oldestKey);
    thumbnailCache.delete(oldestKey);
    thumbnailCacheBytes -= oldest.length * 2;
    releaseRenderedThumbnail(oldestKey);
  }
  return true;
}

function cachedThumbnail(key) {
  const value = thumbnailCache.get(key);
  if (value === undefined) return undefined;
  thumbnailCache.delete(key);
  thumbnailCache.set(key, value);
  return value;
}

function thumbnailMarkup(asset, className = 'photo-art') {
  const key = assetKey(asset);
  return `<div class="photo-placeholder ${className === 'photo-art' ? '' : 'detail-placeholder'}" data-thumbnail-key="${escapeHtml(key)}">${svgImage}</div>`;
}

function observeThumbnailNodes() {
  if (!thumbnailObserver && 'IntersectionObserver' in window) {
    thumbnailObserver = new IntersectionObserver(entries => {
      for (const entry of entries) {
        const node = entry.target;
        const key = node.dataset.thumbnailKey;
        if (!key) continue;
        if (!entry.isIntersecting) {
          const queued = thumbnailPending.get(key);
          if (queued && !queued.started) {
            queued.cancelled = true;
            thumbnailPending.delete(key);
          }
          if (node.matches('img[data-thumbnail-key]')) {
            thumbnailObserver.unobserve(node);
            const placeholder = placeholderNode(key);
            node.replaceWith(placeholder);
            thumbnailObserver.observe(placeholder);
          }
          continue;
        }
        if (node.matches('img[data-thumbnail-key]')) continue;
        const asset = state.assets.find(item => assetKey(item) === key);
        if (!asset || thumbnailPending.has(key)) continue;
        const cached = cachedThumbnail(key);
        if (cached) {
          const image = document.createElement('img');
          image.className = node.closest('.preview-wrap') ? '' : 'photo-art';
          image.dataset.thumbnailKey = key;
          image.loading = 'lazy';
          image.alt = asset.name;
          image.src = `data:image/jpeg;base64,${cached}`;
          thumbnailObserver.unobserve(node);
          node.replaceWith(image);
          thumbnailObserver.observe(image);
          continue;
        }
        const job = { key, id: asset.id, source: asset.source, started: false, cancelled: false };
        thumbnailPending.set(key, job);
        thumbnailQueue.push(job);
      }
      processThumbnails();
    }, { rootMargin: '360px 0px' });
  }
  if (!thumbnailObserver) {
    for (const asset of state.assets) {
      const key = assetKey(asset);
      if (thumbnailCache.has(key) || thumbnailPending.has(key)) continue;
      const job = { key, id: asset.id, source: asset.source, started: false, cancelled: false };
      thumbnailPending.set(key, job);
      thumbnailQueue.push(job);
    }
    processThumbnails();
    return;
  }
  document.querySelectorAll('[data-thumbnail-key]').forEach(node => thumbnailObserver.observe(node));
}

function renderGrid() {
  const items = filterAssets();
  const grid = $('#photoGrid');
  grid.querySelectorAll('[data-thumbnail-key]').forEach(node => thumbnailObserver?.unobserve(node));
  const selectedItems = selectedAssets();
  const selectedQueued = selectedItems.filter(asset => asset.queued).length;
  $('#selectedCount').textContent = `${state.selected.size} 张`;
  $('#selectionHint').textContent = state.page === 'queue' ? '先复核，再确认从原图库删除' : '候选先加入待删除列表，确认后再执行';
  $('#addSelected').disabled = state.selected.size === 0;
  $('#confirmSelected').disabled = selectedQueued === 0;
  $('#confirmSelected').textContent = selectedQueued ? `确认删除 ${selectedQueued} 张` : '确认删除';
  $('#tabAll').textContent = state.page === 'cleanup' ? state.total : state.total || items.length;
  $('#tabPending').textContent = state.page === 'cleanup' ? state.reviewCounts.pending : '';
  $('#tabLater').textContent = state.page === 'cleanup' ? state.reviewCounts.later : '';
  if (!items.length) {
    const message = state.page === 'search' && !state.query
      ? '输入自然语言描述来搜索已索引照片。<br>例如：海边日落、去年秋天的合影。'
      : state.page === 'cleanup'
        ? state.photoStatus !== 'authorized' && state.photoStatus !== 'limited' && state.source !== 'usb'
          ? '允许访问 Mac 照片图库后即可扫描。<br>你也可以连接并解锁 iPhone，再切换照片来源。'
          : state.reviewCounts.pending + state.reviewCounts.later
            ? '当前筛选下没有照片。'
            : '这条规则还没有扫描结果。<br>点击右上角“扫描图库”开始本地扫描。'
      : state.page === 'queue' ? '待删除列表为空。<br>先在照片或清理建议中选择候选照片。'
        : state.page === 'similar' ? '还没有相似照片结果。<br>先打开一张已建立向量索引的照片，在详情中点击“查找相似照片”。'
          : state.page === 'search' ? '搜索已索引照片。<br>扫描图库后即可进行自然语言语义搜索。' : '当前视图没有照片。';
    grid.innerHTML = `<div class="empty-state"><div class="empty-icon">${svgImage}</div>${message}</div>`;
  } else {
    grid.innerHTML = items.map(asset => {
      const key = localSelectionId(asset);
      const selected = state.selected.has(key);
      const score = Number.isFinite(asset.score)
        ? `${Math.round(asset.score * 100)}% ${state.page === 'similar' ? '相似' : state.page === 'search' ? '相关' : '匹配'}`
        : '';
      const dimensions = asset.width && asset.height ? `${asset.width} × ${asset.height}` : sourceName(asset.source);
      const thumb = thumbnailMarkup(asset);
      return `<article class="photo-card ${selected ? 'selected' : ''}" data-id="${escapeHtml(key)}" tabindex="0" aria-label="${escapeHtml(asset.name)}"><div class="image-wrap">${thumb}<span class="check">${checkSvg}</span>${score ? `<span class="match-pill">${score}</span>` : ''}</div><div class="photo-meta"><span class="photo-date">${escapeHtml(formatDate(asset.date))}</span><span class="photo-size">${escapeHtml(dimensions)}</span></div><div class="photo-reason">${escapeHtml(asset.name)} · ${escapeHtml(sourceName(asset.source))}${asset.queued ? ' · 待删除' : ''}</div></article>`;
    }).join('');
  }
  const canLoadMore = state.page === 'all'
    ? (state.source === 'usb'
      ? state.usbOffset < state.usbViewTotal
      : state.source === 'photos'
        ? state.photoOffset < state.photoTotal
        : state.photoOffset < state.photoTotal || state.usbOffset < state.usbViewTotal)
    : ['cleanup', 'queue'].includes(state.page) && state.offset + state.limit < state.total;
  $('#loadMore').parentElement.style.display = canLoadMore ? 'flex' : 'none';
  const loadedCount = state.page === 'all'
    ? state.source === 'all' ? state.photoOffset + state.usbOffset
      : state.source === 'usb' ? state.usbOffset : state.photoOffset
    : Math.min(state.offset + state.limit, state.total);
  $('#loadMore').textContent = `加载更多（${loadedCount} / ${state.total}）　↓`;
  updateSortButton();
  decoratePage();
  const focusAsset = items.find(asset => localSelectionId(asset) === state.focus) || items[0];
  renderInspector(focusAsset || null);
  observeThumbnailNodes();
  $('.inspector').classList.toggle('is-hidden', state.inspectorHidden);
  $('.selection-bar').classList.toggle('inspector-hidden', state.inspectorHidden);
  $('#showInspector').classList.toggle('visible', state.inspectorHidden);
}

function renderInspector(asset) {
  $('#inspectorBody').querySelectorAll('[data-thumbnail-key]').forEach(node => thumbnailObserver?.unobserve(node));
  if (!asset) {
    $('#inspectorBody').innerHTML = `<div class="empty-state" style="padding:54px 12px"><div class="empty-icon">${svgImage}</div>选择一张照片查看信息</div>`;
    return;
  }
  const thumb = thumbnailMarkup(asset);
  const rule = state.rules.find(item => item.id === asset.rule_id);
  const score = Number.isFinite(asset.score) ? asset.score : null;
  const similarity = state.page === 'similar' ? state.similarAnchor : null;
  const semanticSearch = state.page === 'search';
  const matchTitle = similarity ? `与「${similarity.name}」相似` : semanticSearch ? '语义搜索结果' : rule?.name || '尚未匹配扫描规则';
  const matchReason = similarity
    ? '基于本地图片向量计算的相似度，仅比较已建立索引的照片。'
    : semanticSearch ? `与“${state.query}”的语义相关度。`
      : rule?.prompt || '完成规则扫描后，会显示这张照片与描述的匹配分数。';
  const scoreTitle = similarity ? '向量相似度' : semanticSearch ? '语义相关度' : '语义匹配分数';
  $('#inspectorBody').innerHTML = `<div class="preview-wrap">${thumb}</div><div class="image-caption"><strong>${escapeHtml(formatDate(asset.date))}</strong><span>${escapeHtml(sourceName(asset.source))}</span></div><div class="detail-title">${similarity ? '相似度结果' : rule || semanticSearch ? '匹配结果' : '照片信息'}</div><div class="rule-hit"><div class="hit-top"><i class="hit-badge"></i>${escapeHtml(matchTitle)}<span class="hit-score">${score === null ? '未扫描' : `${Math.round(score * 100)}%`}</span></div><div class="hit-reason">${escapeHtml(matchReason)}</div></div>${score === null ? '' : `<div class="score-label"><span>${scoreTitle}</span><strong>${score.toFixed(3)}</strong></div><div class="score-track"><i style="width:${Math.max(0, Math.min(100, score * 100))}%"></i></div>`}<div class="metadata"><span>文件名称</span><strong title="${escapeHtml(asset.name)}">${escapeHtml(asset.name)}</strong><span>拍摄时间</span><strong>${escapeHtml(formatDate(asset.date))}</strong><span>分辨率</span><strong>${asset.width && asset.height ? `${asset.width} × ${asset.height}` : '未知'}</strong><span>来源</span><strong>${escapeHtml(sourceName(asset.source))}</strong><span>审核状态</span><strong>${asset.queued ? '待删除' : asset.reviewed === 'kept' ? '已保留' : asset.reviewed === 'later' ? '稍后复核' : '待复核'}</strong></div><div class="inspector-actions"><button class="keep-btn" id="keepOne">✓　保留</button><button class="later-btn" id="laterOne">稍后复核</button></div><button class="similar-action" id="findSimilar">⌕　查找相似照片</button>`;
  $('#keepOne').onclick = () => review([asset], 'keep');
  $('#laterOne').onclick = () => review([asset], asset.reviewed === 'later' ? 'pending' : 'later');
  $('#findSimilar').onclick = () => findSimilarPhotos(asset);
}

async function findSimilarPhotos(asset) {
  try {
    const rows = await invoke('similar_photos', {
      id: asset.id, anchorSource: asset.source, source: state.source, limit: 120,
    });
    state.page = 'similar';
    state.similarAnchor = asset;
    state.tab = 'all';
    state.query = '';
    state.offset = 0;
    state.total = rows.length;
    state.assets = rows.map(row => ({ ...row, date: row.date, media_type: row.media_type || 'image' }));
    state.selected.clear();
    state.focus = null;
    $('#search').value = '';
    loadPhotoThumbs(state.assets);
    renderGrid();
  } catch (error) {
    toast(`查找相似照片失败：${error}`);
  }
}

async function fetchNativeAssets(source, offset = 0) {
  const nativeSource = source === 'all' ? 'photos' : source;
  const response = await invoke('list_assets', { args: {
    page: state.page, source: nativeSource,
    ruleId: state.page === 'cleanup' ? state.ruleId : null,
    query: null, offset, limit: state.limit,
  } });
  const rows = response.assets || [];
  let usbTotal = state.usbViewTotal;
  if (state.source === 'all' && state.page !== 'queue' && state.page !== 'cleanup' && offset === 0) {
    const usb = await invoke('list_assets', { args: { page: state.page, source: 'usb', ruleId: null, query: null, offset: 0, limit: state.limit } });
    const usbRows = usb.assets || [];
    const visibleUsbRows = state.page === 'favorites' ? usbRows.filter(asset => asset.favorite) : usbRows;
    rows.push(...visibleUsbRows);
    usbTotal = state.page === 'favorites' ? visibleUsbRows.length : usb.total ?? usbRows.length;
    state.usbViewTotal = usbTotal;
  }
  loadPhotoThumbs(rows);
  state.offset = offset;
  state.photoTotal = response.total ?? rows.length;
  state.total = state.source === 'all' ? state.photoTotal + usbTotal : state.photoTotal;
  state.assets = state.page === 'all' && offset ? [...state.assets, ...rows] : rows;
  for (const key of [...state.selected]) if (!state.assets.some(asset => localSelectionId(asset) === key)) state.selected.delete(key);
  renderGrid();
}

function processThumbnails() {
  while (activeThumbnailJobs < 3 && thumbnailQueue.length) {
    activeThumbnailJobs++;
    const job = thumbnailQueue.shift();
    job.started = true;
    (async () => {
      if (job.cancelled) return;
      try {
        for (let attempt = 0; attempt < 12; attempt++) {
          if (job.cancelled) return;
          const thumb = await invoke('load_thumbnail', { id: job.id, source: job.source });
          if (thumb) {
            rememberThumbnail(job.key, thumb);
            if (!job.cancelled && state.assets.some(asset => assetKey(asset) === job.key)) renderGrid();
            return;
          }
          await new Promise(resolve => setTimeout(resolve, 500));
        }
      } catch { /* 权限或设备状态变化时下次显示该照片再尝试。 */ }
      finally {
        if (thumbnailPending.get(job.key) === job) thumbnailPending.delete(job.key);
      }
    })().finally(() => { activeThumbnailJobs--; processThumbnails(); });
  }
}

async function syncAll() {
  try {
    const data = await invoke('bootstrap');
    state.photoStatus = data.photos?.status || 'unknown';
    state.usbStatus = data.usb?.status || 'disconnected';
    state.rules = data.rules || [];
    state.counts = data.counts || state.counts;
    if (!state.rules.some(rule => rule.id === state.ruleId)) state.ruleId = state.rules[0]?.id || null;
    updateRuleNav(); updateHeader(); updateSourceDialog(data.usb);
    if (state.page === 'all') state.total = state.source === 'usb' ? state.counts.usb : state.counts.photos;
    if (state.photoStatus === 'authorized' || state.photoStatus === 'limited') $('#libraryStatus').textContent = `${state.photoStatus === 'limited' ? '有限访问' : '已授权'} · ${data.photos?.count || 0} 张照片`;
    renderGrid();
  } catch (error) {
    updateHeader(); renderGrid(); toast(`连接本地图库失败：${error}`);
  }
}

function loadPhotoThumbs(assets) {
  for (const asset of assets) {
    const key = assetKey(asset);
    if (asset.thumbnail) rememberThumbnail(key, asset.thumbnail);
    asset.thumbnail = null;
  }
}

function updateRuleNav() {
  const container = $('#rulesList');
  container.innerHTML = state.rules.map((rule, index) => `<button class="nav-item rule-nav ${index ? '' : ''}" data-rule="${escapeHtml(rule.id)}"><span class="rule-dot ${['','mauve','olive','blue'][index % 4]}"></span><span class="rule-name">${escapeHtml(rule.name)}</span><span class="nav-count">${rule.candidates || 0}</span></button>`).join('');
  for (const button of container.querySelectorAll('.rule-nav')) button.onclick = () => {
    state.ruleId = button.dataset.rule; state.page = 'cleanup'; state.tab = 'all'; state.offset = 0; state.selected.clear();
    document.querySelectorAll('.rule-nav').forEach(item => item.classList.toggle('active-rule', item === button));
    renderGrid(); reloadCurrent();
  };
  decoratePage();
}

function openRuleModal(rule = null) {
  const modal = $('#ruleModal');
  modal.dataset.ruleId = rule?.id || '';
  $('#ruleTitle').textContent = rule ? '编辑扫描规则' : '创建扫描规则';
  $('#ruleDescription').textContent = rule
    ? '调整照片描述、排除条件和匹配门槛。保存后会重新扫描当前来源。'
    : '用自然语言描述要查找的照片。匹配度只用于排序；扫描结果会进入待复核队列，不会自动删除。';
  $('#ruleName').value = rule?.name || '';
  $('#rulePrompt').value = rule?.prompt || '';
  $('#ruleExclude').value = rule?.exclude_prompt || '';
  $('#ruleThreshold').value = String(rule?.threshold ?? 0.34);
  $('#thresholdValue').textContent = Number($('#ruleThreshold').value).toFixed(2);
  $('#saveRule').textContent = rule ? '保存并重新扫描' : '保存并扫描';
  modal.classList.add('open');
}

function closeRuleModal() {
  const modal = $('#ruleModal');
  modal.classList.remove('open');
  modal.dataset.ruleId = '';
  $('#ruleTitle').textContent = '创建扫描规则';
  $('#ruleDescription').textContent = '用自然语言描述要查找的照片。匹配度只用于排序；扫描结果会进入待复核队列，不会自动删除。';
  $('#ruleName').value = '';
  $('#rulePrompt').value = '';
  $('#ruleExclude').value = '';
  $('#ruleThreshold').value = '0.34';
  $('#thresholdValue').textContent = '0.34';
  $('#saveRule').textContent = '保存并扫描';
}

function updateSourceDialog(usb) {
  $('#photosAccessLabel').textContent = state.photoStatus === 'authorized' ? '已允许'
    : state.photoStatus === 'limited' ? '有限访问' : state.photoStatus === 'denied' ? '已拒绝' : state.photoStatus === 'restricted' ? '受系统限制' : '尚未授权';
  $('#photosAccessHint').textContent = state.photoStatus === 'authorized'
    ? '图库已连接，拾光会在本机建立索引。'
    : state.photoStatus === 'limited' ? 'macOS 仅向拾光提供你选中的照片。'
      : state.photoStatus === 'denied' || state.photoStatus === 'restricted'
        ? '请在系统设置的“隐私与安全性 → 照片”中允许拾光访问。'
        : '点击下方按钮后，macOS 会显示照片图库授权提示。';
  $('#refreshSources').textContent = state.photoStatus === 'authorized' || state.photoStatus === 'limited'
    ? '刷新照片来源' : state.photoStatus === 'denied' || state.photoStatus === 'restricted' ? '刷新授权状态' : '允许访问图库';
  state.usbStatus = usb?.status || state.usbStatus;
  $('#usbAccessLabel').textContent = state.usbStatus === 'connected' ? `${usb.device || 'iPhone'} · ${usb.total ?? (usb.assets || []).length} 张可读照片`
    : state.usbStatus === 'indexing' || state.usbStatus === 'searching' ? '正在查找并读取设备' : state.usbStatus === 'unsupported' ? '当前平台不支持' : '未连接';
  $('#usbAccessHint').textContent = usb?.error || (state.usbStatus === 'connected'
    ? '设备保持解锁并连接 USB 时可浏览；能否批量删除取决于设备和系统权限。'
    : '连接 iPhone，解锁屏幕，并在系统提示时选择“信任”。仅展示 ImageCaptureCore 能读取的媒体。');
}

async function reloadCurrent(offset = 0) {
  try {
    if (state.page === 'search') {
      if (state.query.trim()) await searchPhotos(state.query);
      else { state.assets = []; state.total = 0; renderGrid(); }
      return;
    }
    if (state.page === 'similar') {
      if (state.similarAnchor) await findSimilarPhotos(state.similarAnchor);
      else { state.assets = []; state.total = 0; renderGrid(); }
      return;
    }
    if (state.page === 'all' && state.source === 'all') {
      const photos = await invoke('list_assets', { args: { page: 'all', source: 'photos', ruleId: null, query: null, offset: 0, limit: state.limit } });
      const usb = await invoke('list_assets', { args: { page: 'all', source: 'usb', ruleId: null, query: null, offset: 0, limit: state.limit } });
      state.assets = [...(photos.assets || []), ...(usb.assets || [])];
      state.offset = 0; state.photoOffset = (photos.assets || []).length; state.usbOffset = (usb.assets || []).length;
      state.photoTotal = photos.total || 0;
      state.usbViewTotal = usb.total ?? state.usbViewTotal;
      state.total = (photos.total || 0) + state.usbViewTotal;
      loadPhotoThumbs(state.assets); renderGrid(); return;
    }
    if (state.page === 'all' && state.source === 'usb') {
      const response = await invoke('list_assets', { args: { page: 'all', source: 'usb', ruleId: null, query: null, offset: 0, limit: state.limit } });
      const rows = response.assets || [];
      state.assets = rows; state.offset = 0; state.photoOffset = 0; state.usbOffset = rows.length;
      state.total = response.total || 0; state.usbViewTotal = state.total;
      loadPhotoThumbs(rows); renderGrid(); return;
    }
    if (state.page === 'queue') {
      const source = state.source === 'all' ? 'all' : state.source;
      const response = await invoke('list_assets', { args: { page: 'queue', source, ruleId: null, query: null, offset, limit: state.limit } });
      const rows = response.assets || [];
      state.assets = offset ? [...state.assets, ...rows] : rows;
      state.offset = offset;
      state.total = response.total ?? rows.length;
      loadPhotoThumbs(rows); renderGrid(); return;
    }
    if (state.page === 'cleanup') {
      const dbSource = state.source === 'all' ? 'all' : state.source;
      const reviewState = state.tab === 'unreviewed' ? 'pending' : state.tab === 'later' ? 'later' : null;
      const response = await invoke('list_assets', { args: { page: 'cleanup', source: dbSource, ruleId: state.ruleId, reviewState, query: null, offset, limit: state.limit } });
      const rows = response.assets || [];
      state.assets = offset ? [...state.assets, ...rows] : rows;
      state.offset = offset;
      state.total = response.total ?? rows.length;
      state.reviewCounts = { pending: response.pending ?? 0, later: response.later ?? 0 };
      loadPhotoThumbs(rows); renderGrid(); return;
    }
    if (state.page === 'all' && state.source === 'photos') {
      const response = await invoke('list_assets', { args: { page: 'all', source: 'photos', ruleId: null, query: null, offset: 0, limit: state.limit } });
      const rows = response.assets || [];
      state.assets = rows; state.offset = 0; state.photoOffset = rows.length; state.usbOffset = 0;
      state.total = response.total || 0; state.photoTotal = state.total;
      loadPhotoThumbs(rows); renderGrid(); return;
    }
    await fetchNativeAssets(state.source, offset);
  } catch (error) { toast(`读取照片失败：${error}`); }
}

async function loadMoreLibrary() {
  if (state.page === 'cleanup' || state.page === 'queue') {
    const next = state.offset + state.limit;
    if (next < state.total) await reloadCurrent(next);
    return;
  }
  if (state.page !== 'all') return;
  try {
    const sources = state.source === 'all' ? ['photos', 'usb'] : [state.source];
    for (const source of sources) {
      const loaded = source === 'usb' ? state.usbOffset : state.photoOffset;
      const total = source === 'usb' ? state.usbViewTotal : state.photoTotal;
      if (loaded >= total) continue;
      const response = await invoke('list_assets', { args: {
        page: 'all', source, ruleId: null, query: null, offset: loaded, limit: state.limit,
      } });
      const rows = response.assets || [];
      if (!rows.length) continue;
      state.assets.push(...rows);
      if (source === 'usb') state.usbOffset += rows.length;
      else state.photoOffset += rows.length;
      loadPhotoThumbs(rows);
    }
    renderGrid();
  } catch (error) { toast(`加载更多照片失败：${error}`); }
}

async function reloadCounts() {
  try {
    const status = await invoke('app_status');
    if (status.counts) {
      state.counts = {
        ...status.counts,
        // Native library totals come from Photos / ImageCaptureCore. The DB only
        // contains indexed pages, so keep those totals across review-state refreshes.
        photos: state.counts.photos,
        usb: state.counts.usb,
      };
    }
    state.rules = await invoke('list_rules').catch(() => state.rules);
    updateHeader(); updateRuleNav(); setSubheading(); renderGrid();
  } catch { /* 仍可继续操作当前页面。 */ }
}

async function review(assets, action) {
  const ids = assets.map(asset => assetKey(asset));
  try {
    await invoke('set_review_state', { input: { ids, action } });
    toast(action === 'keep' ? `已将 ${assets.length} 张照片标记为保留` : action === 'later' ? '已移至稍后复核' : '已恢复到待复核');
    state.selected.clear(); await reloadCounts(); await refreshCurrentView();
  } catch (error) { toast(`保存复核状态失败：${error}`); }
}

async function addToQueue() {
  if (!state.selected.size) { toast('先选择照片'); return; }
  const selected = selectedAssets();
  if (!selected.length) { toast('所选照片已不在当前页面，请刷新后重试'); return; }
  if (state.page === 'queue') {
    try {
      await invoke('set_review_state', { input: { ids: selected.map(assetKey), action: 'pending' } });
      state.selected.clear(); toast(`已将 ${selected.length} 张照片移出待删除列表`); await reloadCounts(); await refreshCurrentView();
    } catch (error) { toast(`更新待删除列表失败：${error}`); }
    return;
  }
  try {
    await invoke('set_review_state', { input: { ids: selected.map(assetKey), action: 'queue' } });
    state.selected.clear(); toast(`已将 ${selected.length} 张照片加入待删除列表 · 尚未执行删除`); await reloadCounts(); await refreshCurrentView();
  } catch (error) { toast(`加入待删除列表失败：${error}`); }
}

async function runScan(ruleId = state.ruleId) {
  if (!ruleId) { toast('请先创建一条扫描规则'); return; }
  const source = state.source;
  $('#scanBtn').disabled = true; $('#scanBtn').textContent = '扫描中…';
  $('#progressBanner').classList.add('show'); $('#progressText').textContent = '正在准备本地模型…';
  $('#progressFill').style.width = '2%';
  try {
    state.scanTimer = setInterval(async () => {
      try {
        const progress = await invoke('scan_progress');
        $('#progressText').textContent = progress.message || '正在本地分析照片…';
        $('#progressFill').style.width = progress.total ? `${Math.min(100, progress.completed / progress.total * 100)}%` : '4%';
      } catch { /* 扫描仍由 Rust 后台任务执行。 */ }
    }, 700);
    const result = await invoke('scan_rule', { ruleId, source });
    clearInterval(state.scanTimer); state.scanTimer = null;
    $('#progressFill').style.width = '100%';
    $('#progressText').textContent = `扫描完成 · 检查 ${result.scanned} 张照片，发现 ${result.candidates} 张候选`;
    toast(`本地扫描完成：发现 ${result.candidates} 张候选`);
    await syncAll(); await refreshCurrentView();
    setTimeout(() => $('#progressBanner').classList.remove('show'), 4200);
  } catch (error) {
    clearInterval(state.scanTimer); state.scanTimer = null;
    $('#progressText').textContent = `扫描未完成：${error}`; toast(`扫描失败：${error}`);
  } finally {
    $('#scanBtn').disabled = false; $('#scanBtn').innerHTML = '扫描图库 <span>⌘ ↵</span>';
  }
}

async function saveRule() {
  const name = $('#ruleName').value.trim();
  const prompt = $('#rulePrompt').value.trim();
  if (!name || !prompt) { toast('请填写规则名称和查找描述'); return; }
  const id = $('#ruleModal').dataset.ruleId || null;
  try {
    const rule = await invoke('save_rule', { input: {
      id, name, prompt, excludePrompt: $('#ruleExclude').value.trim(),
      threshold: Number($('#ruleThreshold').value), enabled: true,
    } });
    closeRuleModal();
    state.ruleId = rule.id; state.page = 'cleanup'; state.tab = 'all'; await reloadCounts(); renderGrid(); await runScan(rule.id);
  } catch (error) { toast(`保存规则失败：${error}`); }
}

function openDeleteConfirm() {
  const items = selectedAssets().filter(asset => asset.queued);
  if (!items.length) { toast('先从待删除列表选择照片'); return; }
  $('#deleteDescription').textContent = `将从${[...new Set(items.map(asset => sourceName(asset.source)))].join('、')}中删除 ${items.length} 张照片。照片图库操作由 macOS 完成；USB 删除视设备支持情况而定。`;
  $('#deleteList').innerHTML = items.map(asset => `<li>${escapeHtml(asset.name)} · ${escapeHtml(formatDate(asset.date))} · ${escapeHtml(sourceName(asset.source))}</li>`).join('');
  $('#deleteModal').classList.add('open');
}

async function confirmDelete() {
  const items = selectedAssets().filter(asset => asset.queued);
  if (!items.length) { $('#deleteModal').classList.remove('open'); return; }
  $('#confirmDelete').disabled = true; $('#confirmDelete').textContent = '正在交给系统处理…';
  try {
    const result = await invoke('delete_queued', { input: { ids: items.map(assetKey) } });
    $('#deleteModal').classList.remove('open'); toast(result.warning || `系统已处理 ${result.deleted} 张照片`);
    state.selected.clear(); await syncAll(); await reloadCounts(); await refreshCurrentView();
  } catch (error) {
    toast(`删除失败：${error}`);
  }
  finally { $('#confirmDelete').disabled = false; $('#confirmDelete').textContent = '确认删除'; }
}

let searchDebounce;
async function searchPhotos(query) {
  const generation = ++state.searchGeneration;
  if (!query.trim()) {
    state.page = 'search'; state.query = ''; state.similarAnchor = null; state.offset = 0;
    state.total = 0; state.assets = []; state.selected.clear(); state.focus = null; renderGrid(); return;
  }
  state.similarAnchor = null;
  state.query = query; state.page = 'search'; state.tab = 'all'; state.offset = 0; state.assets = [];
  state.total = 0; state.selected.clear(); state.focus = null; renderGrid();
  try {
    const rows = await invoke('semantic_search', { input: { query, limit: 120, source: state.source } });
    if (generation !== state.searchGeneration || state.page !== 'search' || state.query !== query) return;
    state.assets = rows.map(row => ({ ...row, id: row.id, date: row.date, media_type: 'image' }));
    state.total = state.assets.length;
    loadPhotoThumbs(state.assets); renderGrid();
  } catch (error) {
    if (generation === state.searchGeneration) { toast(`语义搜索暂不可用：${error}`); renderGrid(); }
  }
}

async function refreshCurrentView() {
  if (state.page === 'search') {
    if (state.query.trim()) await searchPhotos(state.query);
    else { state.assets = []; state.total = 0; renderGrid(); }
    return;
  }
  await reloadCurrent();
}

document.querySelectorAll('.nav-item[data-page]').forEach(button => button.addEventListener('click', async () => {
  state.searchGeneration++;
  state.page = button.dataset.page; state.tab = 'all'; state.query = ''; resetLibraryOffsets(); state.selected.clear(); $('#search').value = '';
  if (state.page !== 'similar') state.similarAnchor = null;
  renderGrid(); await reloadCurrent();
}));
document.querySelectorAll('.tab').forEach(button => button.addEventListener('click', async () => {
  state.tab = button.dataset.tab;
  state.selected.clear();
  if (state.page === 'cleanup') {
    state.offset = 0;
    state.assets = [];
    await reloadCurrent();
  } else renderGrid();
}));
document.querySelectorAll('.rule-nav').forEach(button => button.addEventListener('click', () => { state.ruleId = button.dataset.rule; state.page = 'cleanup'; state.tab = 'all'; reloadCurrent(); }));
$('#sourceSelect').addEventListener('change', async event => { state.searchGeneration++; state.source = event.target.value; resetLibraryOffsets(); state.assets = []; state.selected.clear(); updateHeader(); await syncAll(); await refreshCurrentView(); });
$('#search').addEventListener('input', event => { clearTimeout(searchDebounce); searchDebounce = setTimeout(() => searchPhotos(event.target.value), 450); });
$('#scanBtn').addEventListener('click', () => runScan());
$('#refreshBtn').addEventListener('click', () => runScan());
$('#rulesBtn').addEventListener('click', () => openRuleModal(currentRule()));
$('#addRule').addEventListener('click', () => openRuleModal());
$('#saveRule').addEventListener('click', saveRule);
$('#cancelRule').addEventListener('click', closeRuleModal);
$('#ruleThreshold').addEventListener('input', event => { $('#thresholdValue').textContent = Number(event.target.value).toFixed(2); });
$('#addSelected').addEventListener('click', addToQueue);
$('#confirmSelected').addEventListener('click', openDeleteConfirm);
$('#viewQueue').addEventListener('click', async () => { state.page = 'queue'; state.tab = 'all'; state.selected.clear(); renderGrid(); await reloadCurrent(); });
$('#confirmDelete').addEventListener('click', confirmDelete);
$('#cancelDelete').addEventListener('click', () => $('#deleteModal').classList.remove('open'));
$('#closeInspector').addEventListener('click', () => { state.inspectorHidden = true; renderGrid(); });
$('#showInspector').addEventListener('click', () => { state.inspectorHidden = false; renderGrid(); });
$('#sourceHelp').addEventListener('click', () => $('#sourceModal').classList.add('open'));
$('#closeSource').addEventListener('click', () => $('#sourceModal').classList.remove('open'));
$('#refreshSources').addEventListener('click', async () => {
  const button = $('#refreshSources');
  button.disabled = true;
  if (state.photoStatus === 'not_determined' || state.photoStatus === 'unknown') {
    try {
      await invoke('photos_authorize');
      $('#photosAccessLabel').textContent = '等待系统选择';
      $('#photosAccessHint').textContent = '请在 macOS 系统对话框中选择允许或限制访问的照片。';
      for (let attempt = 0; attempt < 90; attempt++) {
        await new Promise(resolve => setTimeout(resolve, 1000));
        const photos = await invoke('photos_status');
        if (photos.status !== 'not_determined') {
          state.photoStatus = photos.status;
          await syncAll();
          if (state.photoStatus === 'authorized' || state.photoStatus === 'limited') await refreshCurrentView();
          break;
        }
      }
      updateSourceDialog();
    } catch (error) {
      toast(`读取照片图库授权状态失败：${error}`);
    }
  } else {
    await syncAll();
    await refreshCurrentView();
  }
  button.disabled = false;
});
$('#sortFilter').addEventListener('click', () => { state.sortAsc = !state.sortAsc; renderGrid(); });
$('#dateFilter').addEventListener('click', () => toast('照片按系统提供的拍摄时间排序'));
$('#loadMore').addEventListener('click', loadMoreLibrary);
$('#photoGrid').addEventListener('click', event => {
  const card = event.target.closest('.photo-card');
  if (!card) return;
  const key = card.dataset.id;
  const asset = state.assets.find(item => localSelectionId(item) === key);
  if (!asset) return;
  if (event.target.closest('.check')) state.selected.has(key) ? state.selected.delete(key) : state.selected.add(key);
  state.focus = key; renderGrid(); renderInspector(asset);
});
$('#photoGrid').addEventListener('keydown', event => {
  if (!['Enter', ' '].includes(event.key)) return;
  const card = event.target.closest('.photo-card');
  if (!card) return;
  event.preventDefault();
  if (event.key === ' ') {
    const key = card.dataset.id;
    state.selected.has(key) ? state.selected.delete(key) : state.selected.add(key);
    renderGrid();
    return;
  }
  const asset = state.assets.find(item => localSelectionId(item) === card.dataset.id);
  if (asset) { state.focus = card.dataset.id; renderGrid(); }
});
document.addEventListener('keydown', event => {
  if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === 'k') { event.preventDefault(); $('#search').focus(); }
  if ((event.metaKey || event.ctrlKey) && event.key === 'Enter') { event.preventDefault(); runScan(); }
  if (event.key === 'Escape') document.querySelectorAll('.modal-backdrop.open').forEach(node => node.classList.remove('open'));
  if (event.key === ' ' && !['INPUT','TEXTAREA','BUTTON'].includes(document.activeElement.tagName) && state.focus) { event.preventDefault(); state.selected.has(state.focus) ? state.selected.delete(state.focus) : state.selected.add(state.focus); renderGrid(); }
});
for (const modal of document.querySelectorAll('.modal-backdrop')) modal.addEventListener('click', event => { if (event.target === modal) modal.classList.remove('open'); });

await syncAll();
await reloadCurrent();
setInterval(async () => {
  let refresh = false;
  try {
    const photos = await invoke('photos_status');
    const status = photos?.status || 'unknown';
    if (status !== state.photoStatus) {
      state.photoStatus = status;
      refresh = true;
    }
  } catch { /* 权限状态无法读取时保留当前图库状态。 */ }
  try {
    const usb = await invoke('usb_status');
    if (usb?.status && usb.status !== state.usbStatus) {
      state.usbStatus = usb.status;
      refresh = true;
    }
  } catch { /* 设备轮询失败时保留当前图库状态。 */ }
  if (refresh) {
    await syncAll();
    await refreshCurrentView();
  }
  try {
    const progress = await invoke('scan_progress');
    if (progress.running && !state.scanTimer) $('#progressText').textContent = progress.message;
  } catch { /* 初始 Rust 状态仍在加载时忽略。 */ }
}, 18000);
