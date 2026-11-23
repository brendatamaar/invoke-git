# Technical Decisions

| Tanggal | Keputusan | Alasan | Fase |
|---------|-----------|--------|------|
| 2026-11-20 | Pin GPUI ke Zed tag `v1.29.2` (`2de9e435...`) + mirror patch `async-task` | Release stabil yang teruji di Windows (Win32/DX11/DirectWrite); hindari breakage dari `main` GPUI | P0 |
| 2026-11-20 | Hybrid git engine: `gix` untuk baca, system `git` CLI (≥ 2.40) untuk tulis & jaringan | `gix` cepat & bebas lock untuk status/log/diff, sedangkan CLI menjamin kompatibilitas penuh hooks, kredensial, push, merge, rebase | P1 |
| 2026-11-20 | Pure core (`step(state, event) -> (state, commands)`) + semua IO di `shell`, disk `.git` sebagai source of truth | Memungkinkan property testing (`proptest`) & model-based harness tanpa IO, serta tahan perubahan eksternal dari terminal/IDE | P1 |
| 2026-11-20 | Setiap operasi tulis membawa expected state (`index` id / ref value) & menolak saat `index.lock` aktif | Menegakkan Law 11 & 12 agar tidak pernah menimpa perubahan disk diam-diam | P1 |
| 2026-11-21 | Generator token desain sendiri (`crates/tokens/build.rs` dari `docs/design/tokens.json`) + komponen UI tipis di atas GPUI | Mengisolasi API GPUI pre-1.0 dan memastikan light/dark theme konsisten tanpa nilai warna hard-coded | P1 |
| 2026-11-22 | Patch staging & discard dibangun di `crates/diff` dengan input melalui filter pipeline `gix` (`convert_to_git`) | `core` tetap bebas isi file; file CRLF di Windows (`core.autocrlf=true`) hanya memunculkan diff perubahan nyata sementara `new_blob` menjaga hash byte mentah | P1 |
| 2026-11-22 | Simpan catatan pre-push review & undo stack di satu DB SQLite (`rusqlite`, fitur `bundled`) di folder app data | Tidak mengotori `.git`, tidak butuh instalasi SQLite di Windows, dan undo stack bertahan lintas restart | P1 |
| 2026-11-22 | Kalibrasi default `DiffOptions::max_work` ke `10_000_000` dengan budget `criterion` via `cargo xtask bench-check` | Menjaga worst-case diff di bawah batas cancel 200 ms dan menggagalkan CI otomatis saat regresi kompleksitas | P1 |
| 2026-11-23 | Tunda dogfood manual 2 minggu hingga setelah v0.3 (`P12`) & adopsi workflow per fase dengan `docs/phases/` | Fitur v0.2 & v0.3 tidak terblokir dogfood manual, bisa dikerjakan paralel di worktree, dan tiap sesi hanya membaca 1 file fase | P1 |
| 2026-11-23 | Cherry-pick dan revert hanya dimulai saat repo `Clean`, HEAD sama dengan saat diminta, dan tidak ada perubahan pada file tracked (staged/unstaged); hasil bersih memakai undo `Reversal::Refs`, konflik menjatuhkan undo point dan memakai alur konflik P1 | Harness menemukan `git revert --abort` menimpa edit unstaged yang sudah ada saat pick dimulai (data hilang); undo ref+index konsisten dengan commit (law 8) | P2 |
