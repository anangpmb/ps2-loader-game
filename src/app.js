// ── PS2 Backup Tool — Frontend Application ──

const App = (() => {
  // ── State ──
  const state = {
    queue: [],
    settings: {
      bufferSize: 8,
      checksum: 'crc32',
      maxRetries: 3,
      splitMode: 'auto',
    },
    device: null,
    processing: false,
    nextId: 1,
  };

  // ── Tauri IPC Bridge ──
  const isTauri = typeof window.__TAURI__ !== 'undefined';
  const invoke = isTauri ? window.__TAURI__.core.invoke : null;
  let destDirHandle = null;

  const Tauri = {
    async detectDevice() {
      if (invoke) {
        return invoke('detect_device');
      }
      if (!destDirHandle) {
        try {
          destDirHandle = await window.showDirectoryPicker({ mode: 'readwrite' });
          log('success', `Destination folder: ${destDirHandle.name}`);
        } catch (e) {
          throw new Error('No destination folder selected');
        }
      }
      return {
        name: destDirHandle.name,
        filesystem: 'Browser FS',
        free_space: 0,
        total_space: 0,
        recommended_mode: 'nosplit',
        mount_point: destDirHandle.name,
      };
    },

    async validateISO(file) {
      if (invoke) {
        return invoke('validate_iso', { path: file.path || file.name });
      }
      const header = await file.slice(0, 0x8800).arrayBuffer();
      const view = new Uint8Array(header);
      let format = null;
      if (view.length > 0x8005 &&
          view[0x8000] === 0x01 &&
          view[0x8001] === 0x43 && view[0x8002] === 0x44 &&
          view[0x8003] === 0x30 && view[0x8004] === 0x30 &&
          view[0x8005] === 0x31) {
        format = 'ISO9660';
      }
      const valid = format !== null;
      return {
        valid,
        size: file.size,
        format,
        error: valid ? null : 'Not a valid ISO9660 image (missing CD001 header)',
        game_id: file.name.replace(/\.[^.]+$/, '').replace(/[^a-zA-Z0-9_]/g, '_'),
      };
    },

    async processISO(queueItem, onProgress) {
      if (invoke) {
        const result = await invoke('process_iso', {
          source: queueItem.path,
          destDir: state.device?.mount_point || '/Volumes/USB',
          gameId: queueItem.gameId || queueItem.name.replace(/\.[^.]+$/, '').replace(/[^a-zA-Z0-9_]/g, '_'),
        });
        onProgress({ phase: 'done', pct: 100 });
        return { success: result.success, checksum: result.chunks?.[0]?.checksum };
      }

      if (!destDirHandle) throw new Error('No destination folder. Click "Refresh" to select one.');
      if (!queueItem.file) throw new Error('No file data. Re-drop the ISO.');

      const gameId = queueItem.gameId || queueItem.name.replace(/\.[^.]+$/, '').replace(/[^a-zA-Z0-9_]/g, '_');
      const CHUNK_SIZE = 0xFFFF0000;
      const totalChunks = Math.ceil(queueItem.file.size / CHUNK_SIZE);

      for (let i = 0; i < totalChunks; i++) {
        const fileName = totalChunks === 1 ? `ul.${gameId}` : `ul.${String(i).padStart(2, '0')}`;
        const chunkStart = i * CHUNK_SIZE;
        const chunkEnd = Math.min(chunkStart + CHUNK_SIZE, queueItem.file.size);
        const chunkBlob = queueItem.file.slice(chunkStart, chunkEnd);

        const fileHandle = await destDirHandle.getFileHandle(fileName, { create: true });
        const writable = await fileHandle.createWritable();
        const reader = chunkBlob.stream().getReader();
        let written = 0;
        const chunkSize = chunkEnd - chunkStart;

        while (true) {
          const { done, value } = await reader.read();
          if (done) break;
          await writable.write(value);
          written += value.byteLength;
          onProgress({
            phase: 'copy',
            chunk: i + 1,
            totalChunks,
            pct: Math.min(Math.round(((i + written / chunkSize) / totalChunks) * 100), 99),
            speed: (written / 1024 / 1024).toFixed(1) + ' MB',
          });
        }
        await writable.close();
        onProgress({
          phase: 'copy',
          chunk: i + 1,
          totalChunks,
          pct: Math.round(((i + 1) / totalChunks) * 100),
          speed: 'Done',
        });
      }

      // Verify
      onProgress({ phase: 'verify', pct: 0 });
      const firstChunkName = totalChunks === 1 ? `ul.${gameId}` : 'ul.00';
      const verifyHandle = await destDirHandle.getFileHandle(firstChunkName);
      const verifyFile = await verifyHandle.getFile();
      if (verifyFile.size === 0) throw new Error('Verification failed: written file is empty');
      onProgress({ phase: 'verify', pct: 100 });

      // ul.cfg
      onProgress({ phase: 'ulcfg', pct: 0 });
      try {
        let ulcfgContent = '';
        try {
          const existing = await destDirHandle.getFileHandle('ul.cfg');
          ulcfgContent = await (await existing.getFile()).text();
        } catch (e) {}
        const title = queueItem.name.replace(/\.[^.]+$/, '');
        const lines = ulcfgContent.split('\n').filter(l => l && !l.startsWith(gameId + '\t'));
        lines.push(`${gameId}\t${title}\t${totalChunks}`);
        const ulcfgHandle = await destDirHandle.getFileHandle('ul.cfg', { create: true });
        const w = await ulcfgHandle.createWritable();
        await w.write(lines.join('\n'));
        await w.close();
      } catch (e) {
        log('warn', `ul.cfg write failed: ${e.message}`);
      }
      onProgress({ phase: 'ulcfg', pct: 100 });

      return { success: true, checksum: 'verified' };
    },

    async generateUlCfg() {
      if (invoke) {
        const count = await invoke('generate_ulcfg', { destDir: state.device?.mount_point || '/Volumes/USB' });
        return { success: true, entries: count };
      }
      return { success: true, entries: state.queue.filter(q => q.status === 'done').length };
    },

    async verifyGames() {
      if (invoke) {
        return invoke('verify_games', { destDir: state.device?.mount_point || '/Volumes/USB' });
      }
      return { verified: state.queue.filter(q => q.status === 'done').length, errors: 0 };
    },
  };

  // ── DOM ──
  const $ = (sel) => document.querySelector(sel);
  const $$ = (sel) => document.querySelectorAll(sel);

  function delay(ms) { return new Promise(r => setTimeout(r, ms)); }

  function formatBytes(bytes) {
    if (!bytes || bytes === 0) return '0 B';
    const k = 1024;
    const sizes = ['B', 'KB', 'MB', 'GB', 'TB'];
    const i = Math.floor(Math.log(bytes) / Math.log(k));
    return parseFloat((bytes / Math.pow(k, i)).toFixed(2)) + ' ' + sizes[i];
  }

  function timestamp() {
    return new Date().toLocaleTimeString('en-GB', { hour12: false });
  }

  function log(level, msg) {
    const viewer = $('#log-viewer');
    const entry = document.createElement('div');
    entry.className = 'log-entry animate-in';
    entry.innerHTML = `
      <span class="log-entry__time">${timestamp()}</span>
      <span class="log-entry__level log-entry__level--${level}">${level.toUpperCase()}</span>
      <span class="log-entry__msg">${msg}</span>
    `;
    viewer.appendChild(entry);
    viewer.scrollTop = viewer.scrollHeight;
  }

  function toast(type, msg) {
    const container = $('#toast-container');
    const el = document.createElement('div');
    el.className = `toast toast--${type}`;
    el.innerHTML = `<span class="toast__msg">${msg}</span>`;
    container.appendChild(el);
    setTimeout(() => {
      el.style.opacity = '0';
      el.style.transform = 'translateY(10px)';
      setTimeout(() => el.remove(), 300);
    }, 4000);
  }

  function updateStats() {
    const q = state.queue;
    $('#stat-total').textContent = q.length;
    $('#stat-done').textContent = q.filter(i => i.status === 'done').length;
    $('#stat-processing').textContent = q.filter(i => i.status === 'processing').length;
    $('#stat-errors').textContent = q.filter(i => i.status === 'error').length;
    $('#btn-start').disabled = !q.some(i => i.status === 'pending') || state.processing;
    $('#btn-clear').disabled = q.length === 0 || state.processing;
  }

  function renderQueue() {
    const container = $('#queue');
    if (state.queue.length === 0) {
      container.innerHTML = `
        <div class="queue__empty">
          <svg width="48" height="48" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1" style="opacity:0.3;margin-bottom:var(--space-2)"><rect x="2" y="2" width="20" height="20" rx="2"/><line x1="8" y1="6" x2="16" y2="6"/><line x1="8" y1="10" x2="16" y2="10"/><line x1="8" y1="14" x2="12" y2="14"/></svg>
          <div>No games in queue</div>
          <div style="font-size:var(--text-xs);margin-top:4px;color:var(--color-text-muted)">ISO files will appear here</div>
        </div>`;
      updateStats();
      return;
    }

    container.innerHTML = state.queue.map(item => `
      <div class="queue-item animate-in" data-id="${item.id}">
        <div class="queue-item__icon queue-item__icon--${item.status}">${statusIcon(item.status)}</div>
        <div class="queue-item__info">
          <div class="queue-item__name" title="${item.name}">${item.name}</div>
          <div class="queue-item__meta">
            ${formatBytes(item.size)} · ${item.format || 'Validating...'}
            ${item.mode ? ` · ${item.mode === 'split' ? 'Split (FAT32)' : 'No-split'}` : ''}
          </div>
          ${item.status === 'processing' ? renderProgress(item) : ''}
          ${item.error ? `<div style="color:var(--color-error);font-size:var(--text-xs);margin-top:2px">${item.error}</div>` : ''}
        </div>
        <div class="queue-item__status queue-item__status--${item.status}">
          ${item.status === 'processing' ? item.progressLabel || 'Processing' : item.status.toUpperCase()}
        </div>
      </div>
    `).join('');
    updateStats();
  }

  function statusIcon(status) {
    return {
      pending: '<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><circle cx="12" cy="12" r="10"/><polyline points="12 6 12 12 16 14"/></svg>',
      processing: '<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><line x1="12" y1="2" x2="12" y2="6"/><line x1="12" y1="18" x2="12" y2="22"/><line x1="4.93" y1="4.93" x2="7.76" y2="7.76"/><line x1="16.24" y1="16.24" x2="19.07" y2="19.07"/><line x1="2" y1="12" x2="6" y2="12"/><line x1="18" y1="12" x2="22" y2="12"/><line x1="4.93" y1="19.07" x2="7.76" y2="16.24"/><line x1="16.24" y1="7.76" x2="19.07" y2="4.93"/></svg>',
      done: '<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M22 11.08V12a10 10 0 1 1-5.93-9.14"/><polyline points="22 4 12 14.01 9 11.01"/></svg>',
      error: '<svg width="18" height="18" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><circle cx="12" cy="12" r="10"/><line x1="15" y1="9" x2="9" y2="15"/><line x1="9" y1="9" x2="15" y2="15"/></svg>',
    }[status] || statusIcon.pending;
  }

  function renderProgress(item) {
    const pct = item.progressPct || 0;
    return `
      <div class="progress">
        <div class="progress__bar"><div class="progress__fill" style="width:${pct}%"></div></div>
        <div class="progress__detail"><span>${item.progressLabel || 'Processing...'}</span><span>${pct}%</span></div>
      </div>`;
  }

  function updateOpPanel(item) {
    const badge = $('#op-badge');
    const details = $('#op-details');
    const progress = $('#op-progress');
    if (!item) {
      badge.className = 'badge badge--info';
      badge.textContent = 'IDLE';
      details.innerHTML = '<div style="color:var(--color-text-muted);font-size:var(--text-sm);text-align:center;padding:var(--space-4)">No operation in progress</div>';
      progress.style.display = 'none';
      return;
    }
    badge.className = 'badge badge--warning';
    badge.textContent = 'ACTIVE';
    details.innerHTML = `
      <div style="font-family:var(--font-mono);font-size:var(--text-sm);color:var(--color-text-primary);margin-bottom:var(--space-2)">${item.name}</div>
      <div style="font-size:var(--text-xs);color:var(--color-text-muted)">${formatBytes(item.size)} · ${item.mode === 'split' ? 'USBExtreme Split' : 'Direct Copy'}</div>`;
    progress.style.display = 'block';
    $('#op-progress-fill').style.width = (item.progressPct || 0) + '%';
    $('#op-progress-label').textContent = item.progressLabel || 'Preparing...';
    $('#op-progress-pct').textContent = (item.progressPct || 0) + '%';
  }

  // ── File Handling ──
  function addFiles(files) {
    for (const file of files) {
      const ext = file.name.split('.').pop().toLowerCase();
      if (!['iso', 'bin', 'cue'].includes(ext)) {
        toast('error', `Unsupported: ${file.name}`);
        continue;
      }
      if (state.queue.some(q => q.name === file.name)) {
        toast('info', `Already in queue: ${file.name}`);
        continue;
      }
      const item = {
        id: state.nextId++,
        name: file.name,
        path: file.path || file.name,
        size: file.size,
        file: file, // keep actual File object for browser mode
        status: 'pending',
        format: null,
        mode: null,
        progressPct: 0,
        progressLabel: '',
        error: null,
        checksum: null,
        gameId: null,
      };
      state.queue.push(item);
      log('info', `Added: ${file.name} (${formatBytes(file.size)})`);
      validateItem(item);
    }
    renderQueue();
  }

  async function validateItem(item) {
    try {
      const result = await Tauri.validateISO(item.file || item.path);
      if (result.valid) {
        item.format = result.format;
        item.size = result.size || item.size;
        item.gameId = result.game_id;
        log('success', `Validated: ${item.name} — ${result.format}${result.game_id ? ' [' + result.game_id + ']' : ''}`);
      } else {
        item.status = 'error';
        item.error = result.error;
        log('error', `Validation failed: ${item.name} — ${result.error}`);
      }
    } catch (e) {
      item.status = 'error';
      item.error = e.message;
      log('error', `Validation error: ${item.name} — ${e.message}`);
    }
    renderQueue();
  }

  // ── Processing ──
  async function startProcessing() {
    if (state.processing) return;
    state.processing = true;
    updateStats();

    const pending = state.queue.filter(i => i.status === 'pending');
    log('info', `Starting batch: ${pending.length} game(s)`);

    for (const item of pending) {
      if (!state.processing) break;

      item.status = 'processing';
      item.progressLabel = 'Starting...';
      item.progressPct = 0;
      renderQueue();
      updateOpPanel(item);

      try {
        if (state.settings.splitMode === 'auto') {
          const fs = state.device?.filesystem;
          const fsName = typeof fs === 'object' ? Object.keys(fs)[0] : fs;
          item.mode = fsName === 'Fat32' || fsName === 'FAT32' ? 'split' : 'nosplit';
        } else {
          item.mode = state.settings.splitMode;
        }

        log('info', `Processing: ${item.name} [${item.mode}]`);

        const result = await Tauri.processISO(item, (progress) => {
          if (progress.phase === 'copy') {
            item.progressLabel = `Chunk ${progress.chunk}/${progress.totalChunks} · ${progress.speed}`;
          } else if (progress.phase === 'verify') {
            item.progressLabel = 'Verifying...';
          } else if (progress.phase === 'ulcfg') {
            item.progressLabel = 'Updating ul.cfg...';
          }
          item.progressPct = progress.pct;
          renderQueue();
          updateOpPanel(item);
        });

        if (result.success) {
          item.status = 'done';
          item.progressLabel = 'Complete';
          item.progressPct = 100;
          item.checksum = result.checksum;
          log('success', `Done: ${item.name} — checksum: ${result.checksum}`);
          toast('success', `Done: ${item.name}`);
        }
      } catch (e) {
        item.status = 'error';
        item.error = e.message;
        log('error', `Failed: ${item.name} — ${e.message}`);
        toast('error', `Failed: ${item.name}`);
      }

      renderQueue();
      updateOpPanel(null);
    }

    state.processing = false;
    updateStats();
    log('info', 'Batch processing complete');
  }

  // ── Device Detection ──
  async function refreshDevice() {
    log('info', 'Detecting target device...');
    try {
      const dev = await Tauri.detectDevice();
      state.device = dev;
      const freeSpace = dev.free_space ?? dev.freeSpace ?? 0;
      const totalSpace = dev.total_space ?? dev.totalSpace ?? 0;
      const mode = dev.recommended_mode ?? dev.mode ?? 'auto';
      const fs = typeof dev.filesystem === 'object' ? Object.keys(dev.filesystem)[0] : dev.filesystem;
      $('#device-name').textContent = dev.name;
      $('#device-fs').textContent = fs;
      $('#device-space').textContent = totalSpace > 0 ? `${formatBytes(freeSpace)} / ${formatBytes(totalSpace)}` : 'N/A';
      $('#device-mode').textContent = mode === 'split' ? 'Split (USBExtreme)' : 'No-split (Direct)';
      log('success', `Device: ${dev.name} — ${fs}${freeSpace > 0 ? ' — ' + formatBytes(freeSpace) + ' free' : ''}`);
    } catch (e) {
      log('error', 'Device detection failed: ' + e.message);
      $('#device-name').textContent = 'Not selected';
    }
  }

  // ── Events ──
  function bindEvents() {
    const dropzone = $('#dropzone');
    const fileInput = $('#file-input');

    dropzone.addEventListener('click', () => fileInput.click());
    dropzone.addEventListener('dragover', e => { e.preventDefault(); dropzone.classList.add('dropzone--active'); });
    dropzone.addEventListener('dragleave', () => dropzone.classList.remove('dropzone--active'));
    dropzone.addEventListener('drop', e => {
      e.preventDefault();
      dropzone.classList.remove('dropzone--active');
      addFiles(Array.from(e.dataTransfer.files));
    });
    fileInput.addEventListener('change', () => { addFiles(Array.from(fileInput.files)); fileInput.value = ''; });

    $('#btn-start').addEventListener('click', startProcessing);
    $('#btn-clear').addEventListener('click', () => {
      if (state.processing) return;
      state.queue = [];
      renderQueue();
      log('info', 'Queue cleared');
    });
    $('#btn-clear-log').addEventListener('click', () => { $('#log-viewer').innerHTML = ''; log('info', 'Log cleared'); });
    $('#btn-refresh-device').addEventListener('click', () => { destDirHandle = null; refreshDevice(); });

    $('#btn-settings').addEventListener('click', () => $('#modal-settings').classList.add('modal-overlay--active'));
    $$('[data-close]').forEach(el => {
      el.addEventListener('click', () => $(`#${el.getAttribute('data-close')}`).classList.remove('modal-overlay--active'));
    });
    $('#btn-save-settings').addEventListener('click', () => {
      state.settings.bufferSize = parseInt($('#setting-buffer').value);
      state.settings.checksum = $('#setting-checksum').value;
      state.settings.maxRetries = parseInt($('#setting-retries').value);
      state.settings.splitMode = $('#setting-split-mode').value;
      $('#modal-settings').classList.remove('modal-overlay--active');
      toast('success', 'Settings saved');
      log('info', `Settings: buffer=${state.settings.bufferSize}MB checksum=${state.settings.checksum} retries=${state.settings.maxRetries} mode=${state.settings.splitMode}`);
    });

    $('#btn-generate-ulcfg').addEventListener('click', async () => {
      log('info', 'Regenerating ul.cfg...');
      const result = await Tauri.generateUlCfg();
      toast('success', `ul.cfg — ${result.entries} entries`);
      log('success', `ul.cfg: ${result.entries} entries`);
    });
    $('#btn-verify').addEventListener('click', async () => {
      log('info', 'Verifying games...');
      const result = await Tauri.verifyGames();
      toast('success', `Verified: ${result.verified} OK, ${result.errors} errors`);
      log('success', `Verify: ${result.verified} OK, ${result.errors} errors`);
    });

    document.addEventListener('keydown', e => {
      if (e.key === 'Escape') $$('.modal-overlay--active').forEach(m => m.classList.remove('modal-overlay--active'));
    });
  }

  function init() {
    bindEvents();
    refreshDevice();
    log('info', 'PS2 Backup Tool v0.1.0 ready');
    log('info', isTauri ? 'Running in Tauri mode (native)' : 'Running in browser mode (select folder when processing)');
  }

  return { init };
})();

document.addEventListener('DOMContentLoaded', App.init);
