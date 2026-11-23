# P6 - Commit Search
Status: todo
Depends on: P1
Parallel-safe with: P1b, P2, P3, P4a, P4b, P7, P8, P9, P10, P11

## Goal
Menyediakan pencarian commit berdasarkan pesan, penulis, hash (penuh/prefix), dan path file secara inkremental dan cancellable pada riwayat commit, termasuk repo besar.

## Tasks
- [ ] Tulis unit/property test parser query & matcher pencarian di `crates/core/tests/search_props.rs` dan integration test pada fixture repo di `crates/shell/tests/search.rs`.
- [ ] Tambah model filter pencarian (`SearchQuery`: message, author, hash prefix, path) di `crates/core/src/search.rs` dan implementasikan scan bertahap (paged & cancellable via `gix`) di `crates/shell/src/search.rs`.
- [ ] Tambah search bar + daftar hasil yang langsung melompat/memilih commit pada graph & detail view di `crates/app/src/history_view.rs` dan komponen UI di `crates/ui/` + Windows checklist di Handoff notes.

## Definition of Done
- `cargo xtask check --core` (VPS/CI) atau `cargo xtask check` (Windows) lulus.
- `cargo test -p invoke-core --test search_props` dan `cargo test -p shell --test search` lulus untuk pencarian message, author, hash prefix, path, dan pembatalan di tengah scan.
- Pencarian tetap responsif (off UI thread, bounded hasil/halaman) tanpa memuat seluruh history ke memori.

## Out of scope
- Full-text grep isi file / pickaxe (`-S`/`-G`) dan command palette keyboard (non-goal di `docs/PRD.md`).

## Handoff notes
- Manfaatkan walk halaman commit yang sudah ada di `crates/shell/src/history.rs` dan commit-graph agar scan cepat di repo ratusan ribu commit.
