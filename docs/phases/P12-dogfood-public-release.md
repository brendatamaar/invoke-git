# P12 - Pre-Release Dogfood & Public Release Gate
Status: todo
Depends on: P12a
Parallel-safe with: -

## Goal
Menutup seluruh gate kualitas sebelum rilis open-source publik: dogfooding harian 2 minggu penuh pada Windows (menggantikan GitKraken untuk semua kerja git), kalibrasi akhir performance budgets, pembersihan crate throwaway spike jika perlu, dan persiapan rilis publik.

## Tasks
- [ ] Jalankan seluruh verifikasi otomatis penuh pada Windows (`cargo xtask check`, `cargo xtask bench-check --ui`) dan periksa CI hijau pada semua workflow (`.github/workflows/ci.yml`, `nightly.yml`, `quint.yml`).
- [ ] Jalankan dogfooding manual selama 2 minggu penuh oleh pemilik project di laptop Windows pada repo nyata (v0.1 + v0.2 + v0.3 workflows) dan catat setiap bug/regresi yang ditemukan.
- [ ] Perbaiki semua bug blocker yang muncul selama dogfooding dengan regression test masing-masing.
- [ ] Finalisasi dokumen rilis: perbarui status di `README.md`, pastikan lisensi `LICENSE` (Apache-2.0) & DCO/contributing notes siap, lalu buat release tag & artifact Windows.

## Definition of Done
- `cargo xtask check` dan `cargo xtask bench-check --ui` lulus 100% di Windows serta seluruh workflow CI hijau.
- Pemilik project mengonfirmasi 2 minggu pemakaian harian tanpa kembali ke GitKraken dan nol bug blocker tersisa.
- Checklist rilis open-source publik (v0.3 gate di `docs/PRD.md`) terverifikasi.

## Out of scope
- Fitur *Later* di `docs/PRD.md` (commit signing UI, partial/sparse clone, bisect, hooks viewer, embedded terminal, paid tier).

## Handoff notes
- Keputusan 2026-11-23 (`docs/DECISIONS.md`): dogfooding Task 26 yang semula di akhir v0.1 dipindah ke fase ini setelah v0.3 selesai agar pengembangan v0.2 dan v0.3 tidak terblokir.
- Catatan dari `tasks/025.DONE.md`: pertimbangkan mengencangkan ceiling di `docs/perf-budgets.toml` ke ~3x median runner Windows setelah semua fitur v0.3 masuk.
