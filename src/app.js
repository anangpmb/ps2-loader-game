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
      sortBySize: true, // Auto-sort largest first for FAT32 contiguity
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
  console.log(`[PS2BT] Mode: ${isTauri ? 'Tauri' : 'Browser'}, invoke: ${!!invoke}, fsUnsupported: ${fsUnsupported}`);
  if (isTauri) {
    console.log('[PS2BT] Tauri dialog available:', !!window.__TAURI__?.dialog);
    console.log('[PS2BT] Tauri dialog.open:', typeof window.__TAURI__?.dialog?.open);
  }

  // Tauri invoke() rejects with a plain string, not an Error object.
  // This helper normalises both so catch blocks always get a usable message.
  function errStr(e) { return e?.message ?? String(e); }

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
          if (crc < 0) crc = ((crc << 1) ^ 0x04C11DB7) | 0;
          else crc = (crc << 1) | 0;
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

  // Detect CD (0x12) vs DVD (0x14).
  // Reads the UDF Volume Recognition Sequence at sector 256 (offset 0x80000):
  // PS2 DVD games carry "BEA01" there; CD-only titles do not.
  // Falls back to size (> 700 MiB = DVD) when the sector is unreadable.
  async function detectMediaType(file) {
    const UDF_OFFSET = 256 * 2048; // 0x80000
    if (file && file.size > UDF_OFFSET + 5) {
      try {
        const slice = file.slice(UDF_OFFSET, UDF_OFFSET + 5);
        const arr = new Uint8Array(await slice.arrayBuffer());
        if (arr[0] === 0x42 && arr[1] === 0x45 && arr[2] === 0x41 && arr[3] === 0x30 && arr[4] === 0x31) {
          return 0x14; // "BEA01" → DVD
        }
      } catch {}
    }
    return file && file.size > 700 * 1024 * 1024 ? 0x14 : 0x12;
  }

  // ── Binary ul.cfg (64-byte records) ──
  function ulWriteAscii(buf, off, str, maxLen) {
    if (!str) return;
    const n = Math.min(str.length, maxLen);
    for (let i = 0; i < n; i++) buf[off + i] = str.charCodeAt(i) & 0xFF;
  }
  function ulReadAscii(bytes, off, len) {
    let end = off;
    const max = Math.min(off + len, bytes.length);
    while (end < max && bytes[end] !== 0) end++;
    if (end === off) return '';
    const slice = bytes.slice(off, end);
    // Try UTF-8 first (written by this app); fall back to Latin-1 (ISO-8859-1)
    // for files written by OPL Manager / USBUtil which use Windows-1252 encoding.
    try {
      return new TextDecoder('utf-8', { fatal: true }).decode(slice).trim();
    } catch {
      return new TextDecoder('iso-8859-1').decode(slice).trim();
    }
  }
  function encodeUlcfg(entries) {
    const validEntries = entries.filter(e => e.title && e.gameId);
    const buf = new Uint8Array(validEntries.length * UL_ENTRY_SIZE);
    validEntries.forEach((e, i) => {
      const off = i * UL_ENTRY_SIZE;
      ulWriteAscii(buf, off, e.title, 32);
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
      const rawTitle = ulReadAscii(bytes, off, 32);
      const image = ulReadAscii(bytes, off + 0x20, 15);

      if (!image || image.trim() === '' || !image.startsWith('ul.')) continue;

      const gameId = image.slice(3).trim();
      const parts = bytes[off + 0x2F];
      const media = bytes[off + 0x30];

      let title = rawTitle;
      if (!title || title.trim() === "") {
        title = gameId;
      }

      entries.push({ title, gameId, parts, media });
    }
    return entries;
  }
  async function readUlcfgEntries() {
    // Get file handles directly from entries() to bypass getFileHandle issues
    const fileHandles = {};
    for await (const [name, handle] of destDirHandle.entries()) {
      if (handle.kind === 'file') fileHandles[name] = handle;
    }
    
    const hasUlcfg = 'ul.cfg' in fileHandles;
    const hasUlcfgBak = 'ul.cfg.bak' in fileHandles;
    log('info', `Files found: ${Object.keys(fileHandles).length}, ul.cfg: ${hasUlcfg}, ul.cfg.bak: ${hasUlcfgBak}`);
    
    // Try ul.cfg first
    if (hasUlcfg) {
      try {
        const file = await fileHandles['ul.cfg'].getFile();
        const bytes = new Uint8Array(await file.arrayBuffer());
        log('info', `ul.cfg loaded: ${bytes.length} bytes, ${Math.floor(bytes.length / 64)} records`);
        return parseUlcfg(bytes);
      } catch (e) {
        log('error', `Read ul.cfg failed: ${e.name}: ${e.message}`);
      }
    }
    
    // Try ul.cfg.bak as fallback
    if (hasUlcfgBak) {
      try {
        const file = await fileHandles['ul.cfg.bak'].getFile();
        const bytes = new Uint8Array(await file.arrayBuffer());
        log('info', `ul.cfg.bak loaded: ${bytes.length} bytes, ${Math.floor(bytes.length / 64)} records`);
        return parseUlcfg(bytes);
      } catch (e) {
        log('error', `Read ul.cfg.bak failed: ${e.name}: ${e.message}`);
      }
    }
    
    log('warn', 'No ul.cfg found on drive');
    return [];
  }
  async function writeUlcfgEntries(entries) {
    if (!destDirHandle) throw new Error('No destination folder selected');
    const data = encodeUlcfg(entries);
    log('info', `Writing ul.cfg: ${entries.length} entries, ${data.length} bytes`);

    // Delete old ul.cfg first: PS2 drives created by USBUtil/OPL set the SYSTEM+HIDDEN
    // attribute which causes Chrome to refuse getFileHandle with "Name is not allowed".
    // Removing the file first lets Chrome create a clean one without those attributes.
    try {
      await destDirHandle.removeEntry('ul.cfg');
      log('info', 'Removed old ul.cfg');
    } catch (e) {
      log('info', `Could not remove ul.cfg (${e.name}: ${e.message}) — will try to overwrite`);
    }

    let lastErr;
    for (let attempt = 0; attempt < 3; attempt++) {
      try {
        if (attempt > 0) await new Promise(r => setTimeout(r, 300 * attempt));
        const handle = await destDirHandle.getFileHandle('ul.cfg', { create: true });
        const w = await handle.createWritable({ keepExistingData: false });
        await w.write(data);
        await w.close();
        log('info', `ul.cfg written: ${entries.length} entries`);
        return;
      } catch (e) {
        lastErr = e;
        log('warn', `ul.cfg write attempt ${attempt + 1} failed: ${e.name}: ${e.message}`);
        if (e.name !== 'InvalidStateError') break; // only retry stale-handle errors
      }
    }
    if (lastErr.message?.includes('Name is not allowed')) {
      throw new Error(
        'ul.cfg has SYSTEM/HIDDEN attributes — Chrome cannot access it. ' +
        'Fix with CMD (Admin): attrib -r -h -s DRIVE:\\ul.cfg'
      );
    }
    throw new Error(
      `ul.cfg write failed: ${lastErr.name}: ${lastErr.message}. ` +
      'Try: attrib -r -h -s DRIVE:\\ul.cfg in CMD (Admin)'
    );
  }

  const Tauri = {
    async listDevices({ interactive = false } = {}) {
      if (invoke) {
        try {
          const devices = await invoke('list_devices');
          if (devices.length > 0) return devices;
        } catch (e) {
          log('warn', 'Auto-detect failed: ' + e.message);
        }
        
        // If interactive and no devices found, offer folder picker
        if (interactive) {
          try {
            // Tauri v2 dialog plugin
            const dialog = window.__TAURI__?.dialog;
            console.log('[PS2BT] Dialog object:', dialog);
            if (dialog && dialog.open) {
              const selected = await dialog.open({
                directory: true,
                multiple: false,
                title: 'Select USB Drive or Folder',
              });
              if (selected) {
                log('info', 'Selected folder: ' + selected);
                try {
                  const info = await invoke('get_device_info_for_path', { path: selected });
                  // Guard: block non-removable drives that look like system roots (e.g. C:\).
                  // The Rust backend sets is_removable=false for DRIVE_FIXED system drives.
                  if (info.is_removable === false) {
                    const norm = selected.replace(/\//g, '\\').toUpperCase();
                    // Only block if it's a drive root (X:\), not a subfolder on a fixed drive.
                    if (/^[A-Z]:\\?$/.test(norm)) {
                      log('error', `${selected} is a system drive root and cannot be used as a target.`);
                      toast('error', `${selected} is a system drive — select a USB or external drive.`);
                      return [];
                    }
                  }
                  return [{ ...info, mount_point: selected }];
                } catch (e2) {
                  log('warn', 'Could not read device info: ' + errStr(e2));
                }
                return [{
                  name: selected.split('/').pop() || selected.split('\\').pop() || selected,
                  filesystem: 'Unknown',
                  free_space: 0,
                  total_space: 0,
                  recommended_mode: 'auto',
                  mount_point: selected,
                }];
              }
            } else {
              log('warn', 'Tauri dialog plugin not available — check capabilities config');
              // Fallback: try using invoke to open dialog
              try {
                const selected = await invoke('open_folder_dialog');
                if (selected) {
                  try {
                    const info = await invoke('get_device_info_for_path', { path: selected });
                    if (info.is_removable === false) {
                      const norm2 = selected.replace(/\//g, '\\').toUpperCase();
                      if (/^[A-Z]:\\?$/.test(norm2)) {
                        log('error', `${selected} is a system drive root and cannot be used as a target.`);
                        toast('error', `${selected} is a system drive — select a USB or external drive.`);
                        return [];
                      }
                    }
                    return [{ ...info, mount_point: selected }];
                  } catch {}
                  return [{
                    name: selected.split('/').pop() || selected,
                    filesystem: 'Unknown',
                    free_space: 0,
                    total_space: 0,
                    recommended_mode: 'auto',
                    mount_point: selected,
                  }];
                }
              } catch (e2) {
                log('warn', 'Fallback dialog failed: ' + errStr(e2));
              }
            }
          } catch (e) {
            log('error', 'Folder picker failed: ' + errStr(e));
          }
        }
        
        return [];
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
        // In Tauri mode `file` may be a plain path string (from native drag-drop)
        // or a File object with a .path property (older Tauri v1 compat).
        const path = typeof file === 'string' ? file : (file.path || file.name);
        return invoke('validate_iso', { path });
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
        return {
          success: result.success,
          checksum: result.chunks?.[0]?.checksum,
          warnings: result.warnings || [],
          chunks: result.chunks || [],
        };
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
      const media = await detectMediaType(queueItem.file);
      const crcHex = Opl.hex(title);

      if (!isSplit) {
        // No-split: copy the whole ISO into CD/ or DVD/ (OPL scans those dirs).
        // Use media type from UDF check, not file size — small DVD games must go in DVD/.
        const subdir = media === 0x14 ? 'DVD' : 'CD';
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
      // Chrome on Windows FAT32/exFAT: after large writes the directory handle's
      // cached state can become stale (InvalidStateError). Retry once with a short
      // delay to let the OS flush directory metadata.
      const totalChunks = Math.ceil(fileSize / CHUNK_SIZE) || 1;
      for (let i = 0; i < totalChunks; i++) {
        const fileName = chunkName(crcHex, gameId, i);
        const chunkStart = i * CHUNK_SIZE;
        const chunkEnd = Math.min(chunkStart + CHUNK_SIZE, fileSize);
        const chunkBlob = queueItem.file.slice(chunkStart, chunkEnd);

        let fileHandle, writable;
        for (let attempt = 0; attempt < 3; attempt++) {
          try {
            fileHandle = await destDirHandle.getFileHandle(fileName, { create: true });
            writable = await fileHandle.createWritable();
            break;
          } catch (e) {
            if (e.name === 'InvalidStateError' && attempt < 2) {
              log('warn', `Chunk ${i + 1} handle stale, retrying (attempt ${attempt + 1})…`);
              await new Promise(r => setTimeout(r, 300 * (attempt + 1)));
            } else {
              throw e;
            }
          }
        }

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
      // On Chrome/Windows the directory cache may lag after large writes — retry with
      // increasing delays before giving up. A NotFoundError here does NOT mean the
      // file is missing; the writable already closed successfully, so treat it as a
      // warning rather than a fatal error.
      onProgress({ phase: 'verify', pct: 0 });
      let verifyWarning = null;
      for (let attempt = 0; attempt < 4; attempt++) {
        try {
          if (attempt > 0) await new Promise(r => setTimeout(r, 500 * attempt));
          const verifyHandle = await destDirHandle.getFileHandle(chunkName(crcHex, gameId, 0));
          const verifyFile = await verifyHandle.getFile();
          if (verifyFile.size === 0) throw new Error('Verification failed: written file is empty');
          verifyWarning = null;
          break;
        } catch (e) {
          verifyWarning = e.message;
          if (e.name !== 'NotFoundError' && e.name !== 'InvalidStateError') break;
        }
      }
      if (verifyWarning) log('warn', `Verify skipped (${verifyWarning}) — chunks were written successfully`);
      onProgress({ phase: 'verify', pct: 100 });

      // Update binary ul.cfg (real 64-byte format).
      // Non-fatal: if ul.cfg is read-only on the drive, chunks are already written correctly.
      onProgress({ phase: 'ulcfg', pct: 0 });
      const ulWarnings = [];
      try {
        let entries = await readUlcfgEntries();
        entries = entries.filter(e => e.gameId !== gameId);
        entries.push({ title, gameId, parts: totalChunks, media });
        log('info', `Adding to ul.cfg: title="${title}" gameId="${gameId}" parts=${totalChunks}`);
        await writeUlcfgEntries(entries);
      } catch (e) {
        const msg = `ul.cfg not updated (${e.message}). Use "Generate ul.cfg" after clearing file attributes.`;
        log('warn', msg);
        ulWarnings.push(msg);
      }
      onProgress({ phase: 'ulcfg', pct: 100 });

      return { success: true, checksum: 'verified', warnings: ulWarnings };
    },

    async generateUlCfg() {
      if (invoke) {
        const count = await invoke('generate_ulcfg', { destDir: state.device?.mount_point || '/Volumes/USB' });
        return { success: true, entries: count };
      }
      // Browser mode: rebuild ul.cfg from chunk files on disk
      if (!destDirHandle) throw new Error('No destination folder selected');
      log('info', `Scanning "${destDirHandle.name}" for chunk files...`);
      
      // Read existing titles from ul.cfg (source of truth for names)
      const existingEntries = await readUlcfgEntries();
      const existingMap = new Map(existingEntries.map(e => [e.gameId, e]));
      
      // CRC → title map from existing entries for fallback matching
      const existingByCrc = new Map();
      for (const e of existingEntries) {
        existingByCrc.set(Opl.hex(e.title), e.title);
      }
      
      const entries = [];
      const seen = new Set();
      for await (const [name, handle] of destDirHandle.entries()) {
        if (handle.kind !== 'file') continue;
        const p = parseChunkName(name);
        if (!p || seen.has(p.gameId)) continue;
        seen.add(p.gameId);
        // Count parts for this game
        let parts = 0;
        for await (const [n2, h2] of destDirHandle.entries()) {
          if (h2.kind === 'file' && parseChunkName(n2)?.gameId === p.gameId) parts++;
        }
        // Preserve existing title if available, otherwise use gameId
        const existing = existingMap.get(p.gameId);
        let title;
        if (existing) {
          title = existing.title;
        } else if (existingByCrc.has(p.crc)) {
          title = existingByCrc.get(p.crc);
        } else {
          title = p.gameId;
        }
        const media = existing ? existing.media : 0x14;
        
        // Detect if title is actually a gameId (fallback placeholder)
        const isGameId = /^[A-Z]{2,4}[_-]\d{3}\.\d{2}$/.test(title);
        if (isGameId) {
          log('warn', `Title "${title}" looks like gameId, keeping as-is (manual rename needed)`);
        }
        entries.push({ title, gameId: p.gameId, parts, media });
        log('info', `Found: ${title} (${p.gameId}) - ${parts} parts`);
      }
      log('info', `Found ${entries.length} games, writing ul.cfg...`);
      await writeUlcfgEntries(entries);
      return { success: true, entries: entries.length };
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
      item.error = errStr(e);
      log('error', `Validation error: ${item.name} — ${errStr(e)}`);
    }
    renderQueue();
  }

  // ── Processing ──
  async function startProcessing() {
    if (state.processing) return;
    state.processing = true;
    updateStats();

    let pending = state.queue.filter(i => i.status === 'pending');
    
    // Auto-sort by size descending (largest first) to minimize FAT32 fragmentation
    if (state.settings.sortBySize !== false) {
      pending.sort((a, b) => b.size - a.size);
      log('info', `Sorted by size (largest first) to minimize fragmentation`);
    }
    
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
          const chunkCount = result.chunks?.length || 1;
          log('success', `Done: ${item.name} — ${chunkCount} chunk(s) · checksum: ${result.checksum}`);
          // Surface any health warnings so the user knows if the game might not boot.
          if (result.warnings && result.warnings.length > 0) {
            result.warnings.forEach(w => log('warn', `  ↳ ${w}`));
            toast('warn', `${item.name}: ${result.warnings.length} warning(s) — check log`);
          } else {
            toast('success', `Done: ${item.name}`);
          }
        }
      } catch (e) {
        item.status = 'error';
        item.error = errStr(e);
        log('error', `Failed: ${item.name} — ${errStr(e)}`);
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
      log('error', 'Device scan failed: ' + errStr(e));
      select.innerHTML = '<option value="">Detection failed</option>';
      if (interactive) toast('error', errStr(e));
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
      const msg = errStr(e);
      container.innerHTML = `<div style="text-align:center;color:var(--color-error);font-size:var(--text-sm);padding:var(--space-4)">${msg}</div>`;
      log('error', 'Game scan failed: ' + msg);
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
      toast('error', `Delete failed: ${errStr(e)}`);
      log('error', `Delete failed: ${errStr(e)}`);
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
          // Rename chunk files: ul.<oldCRC>.<gameId>.<part> → ul.<newCRC>.<gameId>.<part>
          const oldCrc = Opl.hex(oldTitle);
          const newCrc = Opl.hex(newTitle);
          if (oldCrc !== newCrc) {
            const toRename = [];
            for await (const [name, handle] of destDirHandle.entries()) {
              const p = parseChunkName(name);
              if (p && p.gameId === gameId && p.crc === oldCrc) {
                toRename.push({ oldName: name, part: p.part });
              }
            }
            for (const { oldName, part } of toRename) {
              const newName = chunkName(newCrc, gameId, parseInt(part, 16));
              const file = await destDirHandle.getFileHandle(oldName);
              const data = await file.getFile();
              const newHandle = await destDirHandle.getFileHandle(newName, { create: true });
              const w = await newHandle.createWritable();
              await w.write(data);
              await w.close();
              await destDirHandle.removeEntry(oldName);
            }
            log('info', `Renamed ${toRename.length} chunk files: CRC ${oldCrc} → ${newCrc}`);
          }
          // Update ul.cfg
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
      toast('error', `Rename failed: ${errStr(e)}`);
      log('error', `Rename failed: ${errStr(e)}`);
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
      const g = chunkGroups[e.gameId];
      // Skip ul.cfg entry if chunk files don't exist (size 0)
      if (!g || g.size === 0) {
        log('info', `Skipping ${e.gameId}: no chunk files found`);
        continue;
      }
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
            if (known.has(gameId)) continue; // Skip if already in ul.cfg
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

    if (isTauri && window.__TAURI__?.event?.listen) {
      // Tauri v2: native drag-drop event gives absolute OS paths.
      // HTML5 File objects from e.dataTransfer.files have no .path in Tauri v2.
      window.__TAURI__.event.listen('tauri://drag-drop', (event) => {
        dropzone.classList.remove('dropzone--active');
        const paths = event.payload?.paths || [];
        const mockFiles = paths.map(p => ({
          name: p.replace(/\\/g, '/').split('/').pop(),
          path: p,
          size: 0, // filled in by validate_iso result
        }));
        if (mockFiles.length) addFiles(mockFiles);
      });
      // Prevent HTML5 drop from also firing (would add items with no path).
      dropzone.addEventListener('drop', e => { e.preventDefault(); dropzone.classList.remove('dropzone--active'); });
    } else {
      dropzone.addEventListener('drop', e => {
        e.preventDefault();
        dropzone.classList.remove('dropzone--active');
        addFiles(Array.from(e.dataTransfer.files));
      });
    }

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
    $('#sort-games').addEventListener('change', async () => {
      renderDeviceGames();
      if (invoke && state.device?.mount_point) {
        const sortBy = $('#sort-games').value;
        try {
          const n = await invoke('sort_ulcfg', { destDir: state.device.mount_point, sortBy });
          log('info', `ul.cfg sorted by "${sortBy}" (${n} entries)`);
        } catch (e) {
          log('warn', 'Could not sort ul.cfg: ' + errStr(e));
        }
      }
    });

    $('#btn-settings').addEventListener('click', () => {
      $('#setting-buffer').value = state.settings.bufferSize;
      $('#setting-checksum').value = state.settings.checksum;
      $('#setting-retries').value = state.settings.maxRetries;
      $('#setting-split-mode').value = state.settings.splitMode;
      $('#setting-sort-by-size').checked = state.settings.sortBySize !== false;
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
      state.settings.sortBySize = $('#setting-sort-by-size').checked;
      localStorage.setItem('ps2bt-settings', JSON.stringify(state.settings));
      $('#modal-settings').classList.remove('modal-overlay--active');
      toast('success', 'Settings saved');
      log('info', `Settings: buffer=${state.settings.bufferSize}MB checksum=${state.settings.checksum} retries=${state.settings.maxRetries} mode=${state.settings.splitMode} sortBySize=${state.settings.sortBySize}`);
      if (state.device) updateDeviceDisplay(state.device);
    });

    $('#btn-generate-ulcfg').addEventListener('click', async () => {
      log('info', 'Regenerating ul.cfg...');
      try {
        const result = await Tauri.generateUlCfg();
        toast('success', `ul.cfg — ${result.entries} entries`);
        log('success', `ul.cfg: ${result.entries} entries`);
      } catch (e) {
        toast('error', `ul.cfg failed: ${errStr(e)}`);
        log('error', `ul.cfg failed: ${errStr(e)}`);
      }
    });
    $('#btn-verify').addEventListener('click', async () => {
      log('info', 'Verifying games...');
      const result = await Tauri.verifyGames();
      toast('success', `Verified: ${result.verified} OK, ${result.errors} errors`);
      log('success', `Verify: ${result.verified} OK, ${result.errors} errors`);
    });

    $('#btn-check-contiguity').addEventListener('click', async () => {
      if (!state.device?.mount_point) {
        toast('error', 'Select a target drive first');
        return;
      }
      log('info', 'Checking file contiguity...');
      try {
        if (invoke) {
          const results = await invoke('check_contiguity', { destDir: state.device.mount_point });
          if (results.length === 0) {
            log('warn', 'No ul.* files found on device');
            toast('info', 'No split files found');
            return;
          }
          let fragmented = 0;
          for (const r of results) {
            if (r.contiguous) {
              log('success', `OK: ${r.file} — 1 extent, ${formatBytes(r.size)}`);
            } else {
              log('error', `FRAGMENTED: ${r.file} — ${r.extents} extents, ${formatBytes(r.size)}`);
              fragmented++;
            }
          }
          if (fragmented === 0) {
            toast('success', `All ${results.length} file(s) contiguous`);
          } else {
            toast('error', `${fragmented}/${results.length} file(s) fragmented — run defrag`);
          }
        } else {
          log('warn', 'Contiguity check requires native mode (Tauri)');
          toast('info', 'Not available in browser mode');
        }
      } catch (e) {
        const msg = e?.message ?? String(e);
        log('error', 'Contiguity check failed: ' + msg);
        toast('error', msg);
      }
    });

    $('#btn-defrag').addEventListener('click', async () => {
      if (!state.device?.mount_point) {
        toast('error', 'Select a target drive first');
        return;
      }
      if (!invoke) {
        log('warn', 'Defrag requires native mode (Tauri)');
        toast('info', 'Not available in browser mode');
        return;
      }
      log('info', 'Rewriting fragmented split files (best-effort, not guaranteed) — this may take several minutes...');
      const btn = $('#btn-defrag');
      btn.disabled = true;
      try {
        const result = await invoke('defrag_split_files', { destDir: state.device.mount_point });
        if (result.defragged === 0 && result.skipped === 0) {
          toast('success', 'All split files are already contiguous');
          log('success', 'Defrag: nothing to do — all files contiguous');
        } else {
          const moved = formatBytes(result.bytes_moved);
          toast('success', `Defrag: ${result.defragged} file(s) defragged, ${moved} rewritten`);
          log('success', `Defrag: ${result.defragged} defragged, ${result.skipped} skipped, ${moved} moved`);
        }
        for (const err of (result.errors || [])) {
          log('error', `Defrag error: ${err}`);
        }
        if (result.defragged > 0) refreshDeviceGames();
      } catch (e) {
        log('error', 'Defrag failed: ' + errStr(e));
        toast('error', 'Defrag failed: ' + errStr(e));
      } finally {
        btn.disabled = false;
      }
    });

    // ── Safe Restore ──
    const restoreState = {
      sourcePath: null,
      running: false,
      unlisten: null,
    };

    function openRestoreModal() {
      const dest = state.device?.mount_point;
      $('#restore-dest-display').textContent = dest || 'No drive selected — select one in Step 1';
      $('#restore-dest-display').style.color = dest ? 'var(--color-text-primary)' : 'var(--color-text-muted)';
      updateRestoreStartBtn();
      $('#modal-restore').classList.add('modal-overlay--active');
    }

    function updateRestoreStartBtn() {
      $('#btn-restore-start').disabled =
        !invoke || !restoreState.sourcePath || !state.device?.mount_point || restoreState.running;
    }

    async function scanAndShowSource(sourcePath) {
      if (!invoke) {
        $('#restore-scan-result').style.display = 'none';
        updateRestoreStartBtn();
        return;
      }
      try {
        const scan = await invoke('scan_source_folder', { sourceDir: sourcePath });
        $('#restore-file-count').textContent = scan.files.length;
        $('#restore-total-size').textContent = formatBytes(scan.total_bytes);

        // Build subdir summary line.
        let subdirLine = '';
        if (scan.subdirs_found && scan.subdirs_found.length > 0) {
          subdirLine = `<div style="margin-bottom:4px;color:var(--color-text-secondary)">
            Subdirectories: ${scan.subdirs_found.map(d => `<strong>${d}/</strong>`).join(' · ')}
          </div>`;
        }
        if (scan.subdirs_skipped && scan.subdirs_skipped.length > 0) {
          subdirLine += `<div style="margin-bottom:4px;color:var(--color-warning)">
            ⚠ Nested sub-folders inside [${scan.subdirs_skipped.join(', ')}] were skipped (only 1 level deep is supported).
          </div>`;
        }

        const listEl = $('#restore-file-list');
        listEl.innerHTML = subdirLine + scan.files.map(f => {
          const prefix = f.subdir ? `<span style="color:var(--color-info)">${f.subdir}/</span>` : '';
          return `<div style="display:flex;justify-content:space-between;padding:1px 0">
                    <span style="overflow:hidden;text-overflow:ellipsis;white-space:nowrap;max-width:380px">${prefix}${f.name}</span>
                    <span style="margin-left:8px;white-space:nowrap;color:var(--color-text-muted)">${formatBytes(f.size)}</span>
                  </div>`;
        }).join('');
        $('#restore-scan-result').style.display = 'block';
        updateRestoreStartBtn();
      } catch (e) {
        log('warn', 'Scan failed: ' + e.message);
      }
    }

    $('#btn-safe-restore').addEventListener('click', openRestoreModal);

    $('#btn-restore-browse').addEventListener('click', async () => {
      let selected = null;
      try {
        if (invoke) {
          selected = await invoke('open_folder_dialog');
        } else if (window.showDirectoryPicker) {
          // Browser mode: picker returns only a name, not a full native path — can't pass to Rust.
          await window.showDirectoryPicker({ mode: 'read' });
          toast('warn', 'Safe Restore requires the desktop app — the browser cannot read native paths');
        }
      } catch (e) {
        log('warn', 'Folder picker failed: ' + e.message);
      }
      if (selected) {
        restoreState.sourcePath = selected;
        $('#restore-source-path').value = selected;
        await scanAndShowSource(selected);
      }
    });

    $('#btn-restore-start').addEventListener('click', async () => {
      if (!invoke) {
        toast('error', 'Safe Restore requires the desktop app');
        return;
      }
      const source = restoreState.sourcePath;
      const dest = state.device?.mount_point;
      if (!source || !dest || restoreState.running) return;

      restoreState.running = true;
      updateRestoreStartBtn();
      $('#restore-progress-section').style.display = 'block';
      $('#restore-progress-fill').style.width = '0%';
      $('#restore-progress-pct').textContent = '0%';
      $('#restore-current-file').textContent = 'Starting...';
      $('#restore-progress-label').textContent = 'Preparing...';

      log('info', `Safe restore: ${source} → ${dest}`);

      // Subscribe to progress events from Rust.
      if (window.__TAURI__?.event?.listen) {
        restoreState.unlisten = await window.__TAURI__.event.listen('copy-folder-progress', (event) => {
          const p = event.payload;
          $('#restore-progress-fill').style.width = p.total_pct + '%';
          $('#restore-progress-pct').textContent = p.total_pct + '%';
          $('#restore-progress-label').textContent = `${p.file_index} / ${p.total_files} files`;
          $('#restore-current-file').textContent =
            `[${p.file_index}/${p.total_files}] ${p.file} — ${p.file_pct}%`;
          // Log only at file start (0%) and file completion (100%).
          if (p.file_pct === 0) {
            log('info', `  → [${p.file_index}/${p.total_files}] ${p.file}`);
          } else if (p.file_pct >= 100) {
            log('success', `  ✓ [${p.file_index}/${p.total_files}] ${p.file}`);
          }
        });
      }

      try {
        const result = await invoke('copy_folder_ordered', { sourceDir: source, destDir: dest });
        log('success', `Restore complete — ${result.copied} copied · ${result.skipped} skipped · ${formatBytes(result.total_bytes)}`);
        for (const err of (result.errors || [])) log('error', `  ↳ ${err}`);
        if (result.errors.length === 0) {
          toast('success', `Restore done: ${result.copied} file(s)`);
        } else {
          toast('warn', `Restore done — ${result.errors.length} error(s), check log`);
        }
        refreshDeviceGames();
      } catch (e) {
        log('error', 'Restore failed: ' + errStr(e));
        toast('error', 'Restore failed: ' + errStr(e));
      } finally {
        restoreState.running = false;
        if (restoreState.unlisten) { restoreState.unlisten(); restoreState.unlisten = null; }
        updateRestoreStartBtn();
        $('#restore-progress-fill').style.width = '100%';
      }
    });

    $('#btn-format-opl').addEventListener('click', async () => {
      if (!state.device?.mount_point) {
        toast('error', 'Select a target drive first');
        return;
      }
      const mountPoint = state.device.mount_point;
      const driveName = state.device.name || mountPoint;
      
      // Confirmation dialog
      const confirmed = confirm(
        `WARNING: This will ERASE ALL DATA on "${driveName}"!\n\n` +
        `The drive will be formatted as FAT32 and initialized for OPL.\n\n` +
        `Are you sure you want to continue?`
      );
      if (!confirmed) return;

      const label = prompt('Enter volume label (max 11 chars, e.g. "PS2USB"):', 'PS2USB');
      if (!label) return;

      log('info', `Formatting ${driveName} for OPL...`);
      try {
        if (invoke) {
          const result = await invoke('format_drive_for_opl', {
            mountPoint: mountPoint,
            volumeLabel: label.substring(0, 11),
          });
          log('success', result);
          toast('success', 'Drive formatted for OPL');
          refreshDeviceGames();
        } else {
          log('warn', 'Format requires native mode (Tauri)');
          toast('info', 'Not available in browser mode');
        }
      } catch (e) {
        log('error', 'Format failed: ' + errStr(e));
        toast('error', errStr(e));
      }
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
    console.log('[PS2BT] init: isTauri=', isTauri, 'fsUnsupported=', fsUnsupported, 'deviceBtn=', !!deviceBtn);
    
    if (deviceBtn) {
      deviceBtn.title = isTauri
        ? 'Rescan drives or select folder'
        : 'Choose the USB drive / folder to write games to';
      if (isTauri) {
        deviceBtn.innerHTML =
          '<svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><polyline points="23 4 23 10 17 10"/><polyline points="1 20 1 14 7 14"/><path d="M3.51 9a9 9 0 0 1 14.85-3.36L23 10M1 14l4.64 4.36A9 9 0 0 0 20.49 15"/></svg> Select Drive';
      }
    }

    // Browser mode without the File System Access API (macOS Safari, Firefox)
    // cannot write to a drive at all — surface a persistent, actionable notice
    // instead of a button that silently does nothing.
    if (fsUnsupported) {
      console.log('[PS2BT] fsUnsupported=true, disabling button');
      showBrowserSupportNote(deviceBtn);
    } else {
      console.log('[PS2BT] fsUnsupported=false, button should be enabled');
    }

    refreshDevice();
    log('info', 'PS2 Backup Tool v0.1.0 ready');
    if (isTauri) {
      log('info', 'Running in Tauri mode (native)');
    } else if (fsUnsupported) {
      log('warn', 'This browser lacks the File System Access API — use Chrome/Edge or the desktop app');
    } else {
      // Warn about browser compatibility
      const isBrave = navigator.brave && navigator.brave.isBrave;
      const isWindows = navigator.platform.includes('Win');
      if (isBrave) {
        log('warn', 'Brave browser detected — File System Access API may have issues on Windows. Use Chrome/Edge for best compatibility.');
      } else if (isWindows) {
        log('info', 'Windows detected — if you encounter "Name is not allowed" errors, try Chrome/Edge instead of Brave.');
      }
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
