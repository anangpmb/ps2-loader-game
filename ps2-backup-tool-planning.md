# PS2 Backup & Loader Tool — Project Planning

## 1. Ringkasan Proyek

**Tujuan:** Membangun tool desktop cross-platform (Windows & Mac) yang menggabungkan fungsi OPL Manager + USBUtil v2 — menyiapkan game ISO PS2 di HDD/flashdisk agar bisa dimainkan lewat Open PS2 Loader (OPL) di PS2 dengan modchip (mis. Matrix).

**Masalah yang ingin diselesaikan:** USBUtil v2 (tool lama, sudah tidak dikembangkan) sering menghasilkan copy yang corrupt/tidak bisa dimainkan untuk game berukuran >4GB, karena proses split ke format USBExtreme tidak diverifikasi integritasnya.

**Target user:** Pemilik PS2 dengan modchip yang mengonversi/menyalin ISO game ke USB/HDD untuk dimuat lewat OPL.

## 2. Keputusan Arsitektur (Sudah Diputuskan)

| Aspek | Keputusan |
|---|---|
| Platform | Windows & Mac, cross-platform sejak awal |
| Framework | Tauri (Rust backend + Web frontend) |
| Strategi filesystem | Hybrid — default FAT32 + split (USBExtreme), opsional mode NTFS/exFAT tanpa split |

## 3. Fitur Inti (MVP → Later Phase)

### MVP (harus ada)
- Import ISO tunggal, validasi struktur (ISO9660/UDF) sebelum diproses
- Deteksi filesystem device target (FAT32 vs NTFS/exFAT) otomatis
- Split ke format USBExtreme (ul.xxxxxxx) untuk device FAT32, dengan checksum per-chunk + verifikasi ulang setelah tulis
- Mode "no-split": copy ISO utuh untuk device NTFS/exFAT
- Retry otomatis per-chunk kalau checksum tidak cocok
- Auto-generate/update `ul.cfg` dan penamaan file sesuai konvensi OPL
- Log proses per game (sukses/gagal + tahap yang gagal)

### Fase Berikutnya (nice-to-have)
- Batch processing banyak game sekaligus + resume kalau proses terputus
- Dukungan kompresi ZSO (mengecilkan ISO agar lebih banyak game muat di bawah 4GB tanpa split)
- Fetch cover art dari database komunitas OPL
- Manajemen VMC (virtual memory card)
- Library manager (list game yang sudah ada di device, hapus/rename dari GUI)

## 4. Alur Kerja Aplikasi (High-Level Flow)

1. User pilih file ISO sumber (atau banyak sekaligus untuk batch)
2. Validasi ISO (cek header, sector count vs ukuran file aktual)
3. Deteksi filesystem device target
4. Tentukan jalur proses: split (FAT32) atau no-split (NTFS/exFAT) — otomatis, dengan opsi override manual
5. Eksekusi copy/split dengan checksum per-chunk + retry otomatis
6. Generate/update `ul.cfg` dan rename sesuai konvensi OPL
7. (Opsional) Fetch cover art
8. Tampilkan ringkasan hasil + log

## 5. Arsitektur Teknis (Tauri)

- **Rust backend:** file I/O (streaming, bukan load-all-to-memory), perhitungan checksum, parsing ISO, logika split/join, deteksi filesystem lintas-OS
- **Frontend (web):** UI drag-and-drop, progress bar per file, antrian batch, log viewer
- **Komunikasi:** IPC via Tauri commands (invoke dari frontend ke backend Rust)

## 6. Rencana Fase Pengembangan

1. **Core engine (CLI/prototype):** validasi ISO, split + checksum + verifikasi, jalur FAT32 saja
2. **Jalur NTFS/exFAT:** deteksi filesystem otomatis, mode no-split
3. **GUI dasar (Tauri):** alur single-file end-to-end
4. **Batch + resume:** antrian banyak game, checkpoint kalau proses terputus
5. **`ul.cfg` generation + auto-rename + VMC management**
6. **Cover art fetch** (opsional)
7. **Packaging & distribusi:** installer Windows, code-signing/notarization Mac

## 7. Catatan Optimasi & Hal yang Perlu Diperhatikan Saat Pengembangan

### Performa I/O
- Baca/tulis file besar (4–8GB+) wajib streaming dengan buffer chunk (misal 4–16MB), jangan load seluruh file ke memory
- Gunakan async I/O (tokio di Rust) supaya UI tidak freeze saat proses berjalan lama
- Ukuran buffer optimal kemungkinan beda antara USB 2.0 vs 3.0 — perlu benchmark, bukan angka tetap

### Integritas Data (ini akar masalah yang mau diperbaiki)
- Pilih algoritma checksum yang seimbang antara kecepatan dan keandalan (CRC32/xxHash untuk speed, SHA-256 kalau butuh assurance lebih tinggi) — perlu diuji di device nyata untuk lihat trade-off waktu proses
- Verifikasi harus re-read dari device setelah tulis, bukan cuma percaya status "selesai" dari OS
- Retry logic perlu batas maksimal (misal 3x) dengan laporan jelas ke user kalau tetap gagal — hindari infinite loop

### Filesystem Lintas-OS
- Deteksi tipe filesystem device berbeda caranya di Windows (WinAPI/GetVolumeInformation) vs Mac (diskutil/IOKit) — perlu lapisan abstraksi di Rust
- Operasi yang butuh akses raw disk atau format device kemungkinan perlu elevated permission, caranya beda di tiap OS

### Parsing ISO — edge case
- Beberapa ISO PS2 punya struktur non-standar (dummy file, padding) — parser ISO9660/UDF harus toleran, jangan terlalu strict sampai ISO valid malah ditolak
- Pertimbangkan dukungan format lain (misal .bin/.cue) di fase lanjutan

### Kepatuhan Format USBExtreme
- Format `ul.xxxxxxx` punya struktur header spesifik — validasi ulang terhadap source code/spec OPL resmi supaya hasilnya benar-benar terbaca OPL, bukan cuma "asal dipotong tiap 4GB"
- Testing di hardware asli (PS2 + modchip) itu wajib, tidak bisa hanya berdasarkan asumsi teori

### Kompresi ZSO (kalau diimplementasikan)
- Dukungan ZSO tergantung versi build OPL yang dipakai user — perlu deteksi/peringatan kalau build lama tidak mendukungnya

### UX & Keamanan
- Konfirmasi tindakan destruktif (format device ke NTFS/exFAT akan menghapus data) harus jelas, idealnya double-confirm
- Progress reporting granular per tahap, supaya user tahu proses macet di bagian mana kalau berjalan lama

### Packaging & Build
- Tauri butuh setup toolchain berbeda per OS — code signing/notarization untuk Mac dan installer untuk Windows perlu effort tersendiri menjelang rilis

### Testing
- Siapkan korpus ISO uji (ukuran kecil, besar, edge-case) untuk automated test proses split+verify sebelum rilis
- Matriks testing hardware nyata: minimal 1 unit PS2 + modchip + beberapa jenis/kecepatan flashdisk berbeda

## 8. Referensi

- OPL Wiki — usb-mode (ps2homebrew/Open-PS2-Loader)
- popl / libOPL (implementasi Python OPL Manager, referensi fitur split & ul.cfg)
- Source code resmi Open PS2 Loader (ps2homebrew/Open-PS2-Loader) — spek format `ul.cfg` dan USBExtreme
