const fs = require('fs');
const path = require('path');

const UL_ENTRY_SIZE = 64;

function ulReadAscii(bytes, off, len) {
  let end = off;
  const max = Math.min(off + len, bytes.length);
  while (end < max && bytes[end] !== 0) end++;
  const raw = Buffer.from(bytes.slice(off, end)).toString('ascii');
  return raw.replace(/[^\x20-\x7E]/g, '').trim() || 'UNKNOWN_GAME';
}

function crc32(name) {
  const tab = new Uint32Array(256);
  for (let table = 0; table < 256; table++) {
    let crc = (table << 24) | 0;
    for (let i = 0; i < 8; i++) {
      if (crc < 0) crc = (crc << 1) | 0;
      else crc = ((crc << 1) ^ 0x04C11DB7) | 0;
    }
    tab[255 - table] = crc >>> 0;
  }
  
  const buf = new Uint8Array(33);
  const n = Math.min(name.length, 32);
  for (let i = 0; i < n; i++) buf[i] = name.charCodeAt(i) & 0xFF;
  let crc = 0;
  let count = 0;
  do {
    const b = buf[count++];
    const idx = (b ^ ((crc >> 24) & 0xFF)) & 0xFF;
    crc = (tab[idx] ^ (((crc << 8) >>> 0) & 0xFFFFFF00)) | 0;
  } while (buf[count - 1] !== 0 && count <= 32);
  return (crc >>> 0).toString(16).toUpperCase().padStart(8, '0');
}

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

const drivePath = process.argv[2];
if (!drivePath) {
  console.log('Usage: node verify_chunks.js /path/to/usb/drive');
  process.exit(1);
}

console.log(`\nVerifying: ${drivePath}\n`);

// Read ul.cfg
const ulcfgPath = path.join(drivePath, 'ul.cfg');
if (!fs.existsSync(ulcfgPath)) {
  console.log('ERROR: ul.cfg not found');
  process.exit(1);
}

const bytes = fs.readFileSync(ulcfgPath);
const entries = [];
for (let off = 0; off + UL_ENTRY_SIZE <= bytes.length; off += UL_ENTRY_SIZE) {
  const title = ulReadAscii(bytes, off, 32);
  const image = ulReadAscii(bytes, off + 0x20, 15);
  const gameId = image.startsWith('ul.') ? image.slice(3) : image;
  const parts = bytes[off + 0x2F];
  const media = bytes[off + 0x30];
  if (title !== 'UNKNOWN_GAME') entries.push({ title, gameId, parts, media });
}

console.log(`Found ${entries.length} entries in ul.cfg\n`);

// List chunk files on disk
const files = fs.readdirSync(drivePath);
const chunkFiles = {};
for (const file of files) {
  const p = parseChunkName(file);
  if (!p) continue;
  if (!chunkFiles[p.gameId]) chunkFiles[p.gameId] = {};
  chunkFiles[p.gameId][p.crc] = (chunkFiles[p.gameId][p.crc] || 0) + 1;
}

// Verify each entry
let errors = 0;
let verified = 0;

for (const entry of entries) {
  const expectedCrc = crc32(entry.title);
  const diskChunks = chunkFiles[entry.gameId];
  
  if (!diskChunks) {
    console.log(`❌ ${entry.title} (${entry.gameId})`);
    console.log(`   No chunk files found on disk`);
    errors++;
    continue;
  }
  
  const hasExpectedCrc = diskChunks[expectedCrc];
  const hasAnyCrc = Object.keys(diskChunks);
  
  if (hasExpectedCrc) {
    console.log(`✅ ${entry.title} (${entry.gameId})`);
    console.log(`   CRC: ${expectedCrc} (match), Parts: ${hasExpectedCrc}/${entry.parts}`);
    verified++;
  } else {
    console.log(`❌ ${entry.title} (${entry.gameId})`);
    console.log(`   Expected CRC: ${expectedCrc}`);
    console.log(`   Found CRCs: ${hasAnyCrc.join(', ')}`);
    console.log(`   MISMATCH — OPL cannot find chunks!`);
    errors++;
  }
}

console.log(`\n${'='.repeat(60)}`);
console.log(`Result: ${verified} verified, ${errors} errors`);
if (errors > 0) {
  console.log(`\nTo fix: Run "Regenerate ul.cfg" in the app to rebuild with correct CRCs`);
}
