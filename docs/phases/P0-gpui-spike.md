# P0 - GPUI Spike
Status: done
Depends on: -
Parallel-safe with: -

## Goal
Memvalidasi GPUI pada Windows 11 untuk merender diff 50k baris dengan syntax highlighting dan commit graph 100k commit pada 60 fps tanpa blank rows serta input teks/IME yang layak.

## Handoff notes
- Commit kunci: `dc2ee29` (P0-1 scaffold), `7f3801c` (P0-2 `spike-diff`), `674cbe6` (P0-3 `spike-graph`), `b2010f2` (P0-4 commit box & gate sweeps), `21efbe4` (gate scroll runs), `df75df7` (close Phase 0 gate as passed).
- Semua 6 kriteria gate lulus (60 fps di AC power, first paint < 100 ms, IME & text selection bekerja, custom drawing tanpa fork GPUI). Detail pengukuran di `docs/spike-results.md` dan `tasks/P0-1..4.DONE.md`.
- GPUI dipin ke Zed `v1.29.2` (`2de9e435a5d98657f35c260b5f4dd0979df659e6`) dengan mirror patch `async-task` di root `Cargo.toml`. Crate `spike-diff` dan `spike-graph` bersifat throwaway.
