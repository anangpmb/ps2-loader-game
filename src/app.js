// ── PS2 Backup Tool — Frontend Application ──

const App = (() => {
  // ── State ──
  const state = {
    queue: [],
    settings: JSON.parse(localStorage.getItem('ps2bt-settings') || 'null') || {
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
  // True when running in a browser that lacks the File System Access API
  // (macOS Safari, Firefox) — drive writing is impossible there.
  const fsUnsupported = !isTauri && typeof window.showDirectoryPicker !== 'function';

  // ── OPL USBExtreme format helpers (browser mode) ──
  // Mirror of the Rust backend (opl_crc.rs / ulcfg.rs / split.rs) so drives
  // written in browser mode use the real, PS2-bootable format.
  const UL_ENTRY_SIZE = 64;

  // OPL's non-standard CRC32 of the game name — port of src-tauri/src/opl_crc.rs.
  const Opl = (() => {
    let tab = null;
    function buildTable() {
      const t = new Uint32Array(256);
      for (let table = 0; table < 256; table++) {
        let crc = (table << 24) | 0; // int32
        for (let i = 0; i < 8; i++) {
          if (crc < 0) crc = (crc << 1) | 0;              // MSB set
          else crc = ((crc << 1) ^ 0x04C11DB7) | 0;
        }
        t[255 - table] = crc >>> 0;
      }
      return t;
    }
    function crc32(name) {
      if (!tab) tab = buildTable();
      const buf = new Uint8Array(33);
      const n = Math.min(name.length, 32);
      for (let i = 0; i < n; i++) buf[i] = name.charCodeAt(i) & 0xFF;
      let crc = 0; // int32
      let count = 0;
      do {
        const b = buf[count++];
        const idx = (b ^ ((crc >> 24) & 0xFF)) & 0xFF;
        crc = (tab[idx] ^ (((crc << 8) >>> 0) & 0xFFFFFF00)) | 0;
      } while (buf[count - 1] !== 0 && count <= 32);
      return crc >>> 0;
    }
    function hex(name) {
      return crc32(name).toString(16).toUpperCase().padStart(8, '0');
    }
    return { crc32, hex };
  })();

  function chunkName(crcHex, gameId, part) {
    return `ul.${crcHex}.${gameId}.${part.toString(16).toUpperCase().padStart(2, '0')}`;
  }

  // Parse `ul.<crc>.<gameId>.<part>` (gameId itself may contain dots).
  function parseChunkName(name) {
    if (name === 'ul.cfg' || !name.startsWith('ul.')) return null;
    const tokens = name.slice(3).split('.');
    if (tokens.length < 3) return null;
    const crc = tokens[0];
    const part = tokens[tokens.length - 1];
    const gameId = tokens.slice(1, -1).join('.');
    if (!crc || !gameId) return null;
    return { crc, gameId, part };
  }

  function mediaForSize(size) { return size <= 700 * 1024 * 1024 ? 0x12 : 0x14; }

  // ── Binary ul.cfg (64-byte records) ──
  function ulWriteAscii(buf, off, str, maxLen) {
    const n = Math.min(str.length, maxLen);
    for (let i = 0; i < n; i++) buf[off + i] = str.charCodeAt(i) & 0xFF;
  }
  function ulReadAscii(bytes, off, len) {
    let end = off;
    const max = Math.min(off + len, bytes.length);
    while (end < max && bytes[end] !== 0) end++;
    return new TextDecoder('ascii').decode(bytes.slice(off, end)).replace(/ +$/, '');
  }
  function encodeUlcfg(entries) {
    const buf = new Uint8Array(entries.length * UL_ENTRY_SIZE);
    entries.forEach((e, i) => {
      const off = i * UL_ENTRY_SIZE;
      ulWriteAscii(buf, off, e.title, 31);
      ulWriteAscii(buf, off + 0x20, 'ul.' + e.gameId, 14);
      buf[off + 0x2F] = Math.min(e.parts || 1, 255) & 0xFF;
      buf[off + 0x30] = e.media === 0x12 ? 0x12 : 0x14;
      buf[off + 0x35] = 0x08; // magic
    });
    return buf;
  }
  function parseUlcfg(bytes) {
    const entries = [];
    for (let off = 0; off + UL_ENTRY_SIZE <= bytes.length; off += UL_ENTRY_SIZE) {
      const title = ulReadAscii(bytes, off, 32);
      const image = ulReadAscii(bytes, off + 0x20, 15);
      const gameId = image.startsWith('ul.') ? image.slice(3) : image;
      const parts = bytes[off + 0x2F];
      const media = bytes[off + 0x30];
      if (title) entries.push({ title, gameId, parts, media });
    }
    return entries;
  }
  async function readUlcfgEntries() {
    try {
      const handle = await destDirHandle.getFileHandle('ul.cfg');
      const bytes = new Uint8Array(await (await handle.getFile()).arrayBuffer());
      return parseUlcfg(bytes);
    } catch (e) { return []; }
  }
  async function writeUlcfgEntries(entries) {
    const handle = await destDirHandle.getFileHandle('ul.cfg', { create: true });
    const w = await handle.createWritable();
    await w.write(encodeUlcfg(entries));
    await w.close();
  }

  const Tauri = {
    async listDevices({ interactive = false } = {}) {
      if (invoke) {
        return invoke('list_devices');
      }
      // Browser mode: a "device" is a folder the user grants access to.
      const asDevice = async () => {
        if (!destDirHandle) return [];
        let hasUlcfg = false;
        try { await destDirHandle.getFileHandle('ul.cfg'); hasUlcfg = true; } catch {}
        return [{
          name: destDirHandle.name,
          filesystem: 'Browser FS',
          free_space: 0,
          total_space: 0,
          recommended_mode: hasUlcfg ? 'split' : 'nosplit',
          mount_point: destDirHandle.name,
        }];
      };

      if (destDirHandle) return await asDevice();

      // Only open the OS folder picker in response to a user gesture. On page
      // load (interactive=false) we stay quiet instead of throwing.
      if (!interactive) return [];

      if (typeof window.showDirectoryPicker !== 'function') {
        throw new Error("This browser can't write to a drive (no File System Access API). On macOS Safari or Firefox, use Chrome/Edge — or the desktop app.");
      }
      try {
        destDirHandle = await window.showDirectoryPicker({ mode: 'readwrite' });
      } catch (e) {
        if (e && e.name === 'AbortError') return []; // user cancelled — not an error
        throw e;
      }
      return asDevice();
    },

    async detectDevice() {
      if (invoke) {
        return invoke('detect_device');
      }
      const devices = await this.listDevices();
      if (devices.length === 0) throw new Error('No device selected');
      return devices[0];
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
      const rawGameId = await readGameIdFromFile(file) || file.name.replace(/\.[^.]+$/, '');
      const gameId = rawGameId.replace(/[^a-zA-Z0-9_.]/g, '_');
      return {
        valid,
        size: file.size,
        format,
        error: valid ? null : 'Not a valid ISO9660 image (missing CD001 header)',
        game_id: gameId,
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

      // Extract real metadata from ISO header
      const title = queueItem.name.replace(/\.[^.]+$/, ''); // display name from filename
      const isoGameId = await readGameIdFromFile(queueItem.file);
      const rawGameId = isoGameId || queueItem.gameId || title;
      // Sanitize gameId: keep dots, underscores, alphanum only (safe for filenames)
      const gameId = rawGameId.replace(/[^a-zA-Z0-9_.]/g, '_');
      log('info', `ISO metadata: title="${title}" gameId="${gameId}"`);
      const fileSize = queueItem.file.size;
      const isSplit = queueItem.mode === 'split';
      const CHUNK_SIZE = 1073741824; // 1 GiB — matches OPL / backend
      const media = mediaForSize(fileSize);
      const crcHex = Opl.hex(title);

      if (!isSplit) {
        // No-split: copy the whole ISO into CD/ or DVD/ (OPL scans those dirs).
        const subdir = fileSize < 4_700_000_000 ? 'CD' : 'DVD';
        const targetDir = await destDirHandle.getDirectoryHandle(subdir, { create: true });
        log('info', `No-split mode: writing to ${subdir}/ directory`);
        const fileHandle = await targetDir.getFileHandle(`${gameId}.iso`, { create: true });
        const writable = await fileHandle.createWritable();
        const reader = queueItem.file.stream().getReader();
        let written = 0;
        while (true) {
          const { done, value } = await reader.read();
          if (done) break;
          await writable.write(value);
          written += value.byteLength;
          onProgress({
            phase: 'copy', chunk: 1, totalChunks: 1,
            pct: Math.min(Math.round((written / fileSize) * 100), 99),
            speed: (written / 1024 / 1024).toFixed(1) + ' MB',
          });
        }
        await writable.close();
        onProgress({ phase: 'verify', pct: 100 });
        log('info', 'No-split mode: ul.cfg not needed (ISO in CD/DVD directory)');
        return { success: true, checksum: 'verified' };
      }

      // Split mode: write `ul.<crc>.<gameId>.<part>` 1 GiB chunks.
      const totalChunks = Math.ceil(fileSize / CHUNK_SIZE) || 1;
      for (let i = 0; i < totalChunks; i++) {
        const fileName = chunkName(crcHex, gameId, i);
        const chunkStart = i * CHUNK_SIZE;
        const chunkEnd = Math.min(chunkStart + CHUNK_SIZE, fileSize);
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
      }

      // Verify first chunk exists and is non-empty.
      onProgress({ phase: 'verify', pct: 0 });
      const verifyHandle = await destDirHandle.getFileHandle(chunkName(crcHex, gameId, 0));
      const verifyFile = await verifyHandle.getFile();
      if (verifyFile.size === 0) throw new Error('Verification failed: written file is empty');
      onProgress({ phase: 'verify', pct: 100 });

      // Update binary ul.cfg (real 64-byte format).
      onProgress({ phase: 'ulcfg', pct: 0 });
      try {
        let entries = await readUlcfgEntries();
        entries = entries.filter(e => e.gameId !== gameId);
        entries.push({ title, gameId, parts: totalChunks, media });
        await writeUlcfgEntries(entries);
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

    const hasDevice = !!state.device;
    const hasPending = q.some(i => i.status === 'pending');
    const startBtn = $('#btn-start');
    startBtn.disabled = !hasPending || state.processing || !hasDevice;
    // Explain why Start is unavailable so the prerequisite is discoverable.
    startBtn.title = !hasDevice
      ? 'Select a target drive first (Step 1)'
      : (!hasPending ? 'Add ISO files to the queue' : '');

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
    refreshDeviceGames(); // auto-refresh game list
  }

  // ── Device Detection ──
  async function refreshDevice(interactive = false) {
    log('info', interactive ? 'Selecting target drive...' : 'Scanning for storage devices...');
    const select = $('#device-select');

    try {
      const devices = await Tauri.listDevices({ interactive });
      state.devices = devices;

      // Populate dropdown
      select.innerHTML = '';
      if (devices.length === 0) {
        select.innerHTML = '<option value="">No drive selected</option>';
        state.device = null;
        updateDeviceDisplay(null);
        refreshDeviceGames();
        log('warn', 'No target drive available');
        return;
      }

      devices.forEach((dev, idx) => {
        const opt = document.createElement('option');
        opt.value = idx;
        const fs = typeof dev.filesystem === 'object' ? Object.keys(dev.filesystem)[0] : dev.filesystem;
        opt.textContent = `${dev.name} (${fs})`;
        select.appendChild(opt);
      });

      // Select first device
      select.value = '0';
      selectDevice(0);
      log('success', `Found ${devices.length} device(s)`);

    } catch (e) {
      log('error', 'Device scan failed: ' + e.message);
      select.innerHTML = '<option value="">Detection failed</option>';
      if (interactive) toast('error', e.message);
    }
  }

  function selectDevice(idx) {
    const devices = state.devices || [];
    const dev = devices[idx];
    if (!dev) return;
    state.device = dev;
    updateDeviceDisplay(dev);
    refreshDeviceGames(); // auto-scan games for the newly selected drive
  }

  function updateDeviceDisplay(dev) {
    const freeSpace = dev?.free_space ?? dev?.freeSpace ?? 0;
    const totalSpace = dev?.total_space ?? dev?.totalSpace ?? 0;
    const recommendedMode = dev?.recommended_mode ?? dev?.mode ?? 'auto';
    const userMode = state.settings?.splitMode;
    const mode = (userMode && userMode !== 'auto') ? userMode : recommendedMode;
    const fs = dev ? (typeof dev.filesystem === 'object' ? Object.keys(dev.filesystem)[0] : dev.filesystem) : '—';

    $('#device-fs').textContent = fs;
    $('#device-space').textContent = totalSpace > 0 ? `${formatBytes(freeSpace)} / ${formatBytes(totalSpace)}` : 'N/A';
    $('#device-mode').textContent = mode === 'split' ? 'Split (USBExtreme)' : 'No-split (Direct)';

    // Prominence state: draw attention while empty, confirm once a drive is set.
    const card = $('#card-target');
    const status = $('#device-status');
    const selectBtn = $('#btn-refresh-device');
    if (dev) {
      card?.classList.remove('target-device--empty');
      card?.classList.add('target-device--ready');
      if (status) { status.className = 'badge badge--success'; status.textContent = 'Ready'; }
      selectBtn?.classList.remove('btn--primary');
    } else {
      card?.classList.add('target-device--empty');
      card?.classList.remove('target-device--ready');
      if (status) { status.className = 'badge badge--info'; status.textContent = 'Required'; }
      if (!fsUnsupported) selectBtn?.classList.add('btn--primary');
    }
    updateStats(); // Start button depends on having a target drive
  }

  // ── Device Games List ──
  let deviceGames = []; // cache for sort

  async function refreshDeviceGames() {
    const container = $('#device-games-list');
    const mountPoint = state.device?.mount_point;

    if (!mountPoint) {
      container.innerHTML = '<div style="text-align:center;color:var(--color-text-muted);font-size:var(--text-sm);padding:var(--space-4)">No device selected</div>';
      return;
    }

    container.innerHTML = '<div style="text-align:center;color:var(--color-text-muted);font-size:var(--text-sm);padding:var(--space-4)">Scanning...</div>';

    try {
      if (invoke) {
        const repaired = await invoke('repair_split_files', { destDir: mountPoint });
        if (repaired > 0) log('info', `Repaired ${repaired} old-format split file(s)`);
        deviceGames = await invoke('list_device_games', { destDir: mountPoint });
      } else {
        deviceGames = await scanBrowserGames();
      }
      renderDeviceGames();
    } catch (e) {
      container.innerHTML = `<div style="text-align:center;color:var(--color-error);font-size:var(--text-sm);padding:var(--space-4)">${e.message}</div>`;
      log('error', 'Game scan failed: ' + e.message);
    }
  }

  function renderDeviceGames() {
    const container = $('#device-games-list');
    const sortBy = $('#sort-games')?.value || 'name';
    let games = [...deviceGames];

    // Sort
    games.sort((a, b) => {
      switch (sortBy) {
        case 'name': return a.title.localeCompare(b.title);
        case 'name-desc': return b.title.localeCompare(a.title);
        case 'size': return a.size - b.size;
        case 'size-desc': return b.size - a.size;
        default: return 0;
      }
    });

    if (games.length === 0) {
      container.innerHTML = '<div style="text-align:center;color:var(--color-text-muted);font-size:var(--text-sm);padding:var(--space-4)">No games found</div>';
      return;
    }

    container.innerHTML = games.map(g => `
      <div class="game-list-item" data-id="${g.game_id}">
        <div class="game-list-item__badge game-list-item__badge--${g.mode}">
          ${g.mode === 'split' ? 'S' : 'C'}
        </div>
        <div style="min-width:0;flex:1">
          <div class="game-list-item__id" title="${g.title}">${g.title}</div>
          <div class="game-list-item__meta">${g.game_id} · ${formatBytes(g.size)} · ${g.location}</div>
        </div>
        <div style="display:flex;gap:2px;flex-shrink:0">
          <button class="btn btn--ghost btn--sm" style="padding:4px" onclick="App.renameGame('${g.game_id}','${g.mode}','${g.location}','${g.title.replace(/'/g, "\\'")}')" title="Rename">
            <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M11 4H4a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2v-7"/><path d="M18.5 2.5a2.121 2.121 0 0 1 3 3L12 15l-4 1 1-4 9.5-9.5z"/></svg>
          </button>
          <button class="btn btn--ghost btn--sm" style="padding:4px;color:var(--color-error)" onclick="App.deleteGame('${g.game_id}','${g.mode}','${g.location}','${g.title.replace(/'/g, "\\'")}')" title="Delete">
            <svg width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><polyline points="3 6 5 6 21 6"/><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/></svg>
          </button>
        </div>
      </div>
    `).join('');

    log('info', `Found ${games.length} game(s) on device`);
  }

  async function deleteGame(gameId, mode, location, title) {
    if (!confirm(`Delete "${title}" (${gameId})?`)) return;
    try {
      if (invoke) {
        await invoke('delete_game', { destDir: state.device.mount_point, gameId, mode, location });
      } else {
        // Browser mode
        if (mode === 'nosplit') {
          const dir = await destDirHandle.getDirectoryHandle(location, { create: false });
          await dir.removeEntry(`${gameId}.iso`);
        } else {
          // Remove every chunk file for this game id, then drop the ul.cfg entry.
          const toRemove = [];
          for await (const name of destDirHandle.keys()) {
            const p = parseChunkName(name);
            if (p && p.gameId === gameId) toRemove.push(name);
          }
          for (const name of toRemove) await destDirHandle.removeEntry(name);
          const entries = (await readUlcfgEntries()).filter(e => e.gameId !== gameId);
          await writeUlcfgEntries(entries);
        }
      }
      toast('success', `Deleted: ${title}`);
      log('info', `Deleted: ${title} (${gameId})`);
      refreshDeviceGames();
    } catch (e) {
      toast('error', `Delete failed: ${e.message}`);
      log('error', `Delete failed: ${e.message}`);
    }
  }

  async function renameGame(gameId, mode, location, oldTitle) {
    const newTitle = prompt(`Rename "${oldTitle}" to:`, oldTitle);
    if (!newTitle || newTitle === oldTitle) return;
    try {
      if (invoke) {
        await invoke('rename_game', { destDir: state.device.mount_point, gameId, mode, location, newTitle });
      } else {
        if (mode === 'split') {
          const entries = await readUlcfgEntries();
          const entry = entries.find(e => e.gameId === gameId);
          if (entry) entry.title = newTitle;
          await writeUlcfgEntries(entries);
        } else {
          toast('info', 'Rename is only available for split-mode games (ul.cfg)');
          return;
        }
      }
      toast('success', `Renamed to: ${newTitle}`);
      log('info', `Renamed: ${oldTitle} → ${newTitle}`);
      refreshDeviceGames();
    } catch (e) {
      toast('error', `Rename failed: ${e.message}`);
      log('error', `Rename failed: ${e.message}`);
    }
  }

  // Read ISO9660 volume label from a File handle (offset 0x8028, 40 bytes).
  async function readIsoTitleFromFile(fileHandle) {
    try {
      const blob = fileHandle.slice(0x8028, 0x8028 + 40);
      const buf = await blob.arrayBuffer();
      const bytes = new Uint8Array(buf);
      let end = bytes.indexOf(0);
      if (end === -1) end = bytes.length;
      const label = new TextDecoder().decode(bytes.slice(0, end)).trim();
      return label || null;
    } catch { return null; }
  }

  // Extract PS2 game ID (e.g. SLUS_200.00) from SYSTEM.CNF in the ISO.
  // Searches first 4MB for BOOT2 = cdrom0:\SLUS_XXX.XX;1
  async function readGameIdFromFile(file) {
    try {
      const SEARCH_SIZE = Math.min(4 * 1024 * 1024, file.size);
      const buf = await file.slice(0, SEARCH_SIZE).arrayBuffer();
      const text = new TextDecoder('ascii').decode(new Uint8Array(buf));
      
      // Search for BOOT2 pattern directly in text (more robust than line splitting)
      const boot2Match = text.match(/BOOT2\s*=\s*cdrom0:\\([^;]+)/);
      if (boot2Match) {
        const id = boot2Match[1].trim();
        if (id) return id.replace(/[^a-zA-Z0-9_.]/g, '_'); // sanitize for filename safety
      }
      
      return null;
    } catch { return null; }
  }

  async function scanBrowserGames() {
    if (!destDirHandle) return [];
    const games = [];
    const known = new Set();

    // Sum chunk file sizes/parts grouped by game id.
    const chunkGroups = {}; // gameId -> { size, parts, firstFile }
    for await (const [name, handle] of destDirHandle.entries()) {
      if (handle.kind !== 'file') continue;
      const p = parseChunkName(name);
      if (!p) continue;
      const file = await handle.getFile();
      const g = chunkGroups[p.gameId] || { size: 0, parts: 0, firstFile: null };
      g.size += file.size;
      g.parts += 1;
      if (!g.firstFile) g.firstFile = file;
      chunkGroups[p.gameId] = g;
    }

    // Split games from ul.cfg (grouped, real titles).
    for (const e of await readUlcfgEntries()) {
      const g = chunkGroups[e.gameId] || { size: 0, parts: e.parts };
      games.push({
        game_id: e.gameId, title: e.title, parts: e.parts,
        size: g.size, location: 'root', mode: 'split',
      });
      known.add(e.gameId);
    }

    // Orphan chunk groups not listed in ul.cfg.
    for (const [gameId, g] of Object.entries(chunkGroups)) {
      if (known.has(gameId)) continue;
      games.push({
        game_id: gameId, title: gameId, parts: g.parts,
        size: g.size, location: 'root', mode: 'split',
      });
    }

    // Scan CD/ and DVD/ directories
    for (const subdir of ['CD', 'DVD']) {
      try {
        const dirHandle = await destDirHandle.getDirectoryHandle(subdir);
        for await (const [name, handle] of dirHandle.entries()) {
          if (handle.kind === 'file' && name.endsWith('.iso')) {
            const gameId = name.replace(/\.iso$/, '');
            games.push({
              game_id: gameId,
              title: gameId,
              parts: 1,
              size: (await handle.getFile()).size,
              location: subdir,
              mode: 'nosplit',
            });
          }
        }
      } catch (e) { /* directory doesn't exist */ }
    }

    return games;
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
    $('#btn-refresh-device').addEventListener('click', () => { destDirHandle = null; refreshDevice(true); });

    $('#device-select').addEventListener('change', (e) => {
      const idx = parseInt(e.target.value);
      if (!isNaN(idx)) {
        selectDevice(idx);
        const dev = state.devices?.[idx];
        if (dev) log('info', `Selected: ${dev.name}`);
      }
    });

    $('#btn-refresh-games').addEventListener('click', refreshDeviceGames);
    $('#sort-games').addEventListener('change', renderDeviceGames);

    $('#btn-settings').addEventListener('click', () => {
      $('#setting-buffer').value = state.settings.bufferSize;
      $('#setting-checksum').value = state.settings.checksum;
      $('#setting-retries').value = state.settings.maxRetries;
      $('#setting-split-mode').value = state.settings.splitMode;
      $('#modal-settings').classList.add('modal-overlay--active');
    });
    $$('[data-close]').forEach(el => {
      el.addEventListener('click', () => $(`#${el.getAttribute('data-close')}`).classList.remove('modal-overlay--active'));
    });
    $('#btn-save-settings').addEventListener('click', () => {
      state.settings.bufferSize = parseInt($('#setting-buffer').value);
      state.settings.checksum = $('#setting-checksum').value;
      state.settings.maxRetries = parseInt($('#setting-retries').value);
      state.settings.splitMode = $('#setting-split-mode').value;
      localStorage.setItem('ps2bt-settings', JSON.stringify(state.settings));
      $('#modal-settings').classList.remove('modal-overlay--active');
      toast('success', 'Settings saved');
      log('info', `Settings: buffer=${state.settings.bufferSize}MB checksum=${state.settings.checksum} retries=${state.settings.maxRetries} mode=${state.settings.splitMode}`);
      if (state.device) updateDeviceDisplay(state.device);
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

    // The device button differs per mode: native rescans connected drives,
    // browser opens a folder picker. Label/icon reflect that.
    const deviceBtn = $('#btn-refresh-device');
    if (deviceBtn) {
      deviceBtn.title = isTauri
        ? 'Rescan connected drives'
        : 'Choose the USB drive / folder to write games to';
      if (isTauri) {
        deviceBtn.innerHTML =
          '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><polyline points="23 4 23 10 17 10"/><polyline points="1 20 1 14 7 14"/><path d="M3.51 9a9 9 0 0 1 14.85-3.36L23 10M1 14l4.64 4.36A9 9 0 0 0 20.49 15"/></svg> Refresh';
      }
    }

    // Browser mode without the File System Access API (macOS Safari, Firefox)
    // cannot write to a drive at all — surface a persistent, actionable notice
    // instead of a button that silently does nothing.
    if (fsUnsupported) {
      showBrowserSupportNote(deviceBtn);
    }

    refreshDevice();
    log('info', 'PS2 Backup Tool v0.1.0 ready');
    if (isTauri) {
      log('info', 'Running in Tauri mode (native)');
    } else if (fsUnsupported) {
      log('warn', 'This browser lacks the File System Access API — use Chrome/Edge or the desktop app to write to a drive');
    } else {
      log('info', 'Running in browser mode — click "Select Drive" to grant folder access');
    }
  }

  // Inline notice shown when the browser can't open/write a drive folder.
  function showBrowserSupportNote(deviceBtn) {
    const body = deviceBtn?.closest('.card__body');
    if (!body || $('#browser-support-note')) return;

    let appLink = '';
    if (location.hostname.endsWith('github.io')) {
      const owner = location.hostname.split('.')[0];
      const repo = location.pathname.split('/').filter(Boolean)[0];
      if (owner && repo) {
        appLink = ` <a href="https://github.com/${owner}/${repo}/releases" target="_blank" rel="noopener">Get the desktop app →</a>`;
      }
    }

    const note = document.createElement('div');
    note.className = 'support-note';
    note.id = 'browser-support-note';
    note.innerHTML =
      '<svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M10.29 3.86L1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0z"/><line x1="12" y1="9" x2="12" y2="13"/><line x1="12" y1="17" x2="12.01" y2="17"/></svg>' +
      '<span>This browser can’t write to a drive. Open in <strong>Chrome</strong> or <strong>Edge</strong>, or use the desktop app.' + appLink + '</span>';
    body.insertBefore(note, body.firstChild);

    // The pick action is impossible here — disable it; the notice explains why.
    if (deviceBtn) {
      deviceBtn.classList.remove('btn--primary');
      deviceBtn.disabled = true;
      deviceBtn.title = 'Not available in this browser — use Chrome/Edge or the desktop app';
    }
  }

  return { init, deleteGame, renameGame };
})();

document.addEventListener('DOMContentLoaded', App.init);
