# P6 - Commit Search
Status: review
Depends on: P1
Parallel-safe with: P1b, P2, P3, P4a, P4b, P7, P8, P9, P10, P11

## Goal
Menyediakan pencarian commit berdasarkan pesan, penulis, hash (penuh/prefix), dan path file secara inkremental dan cancellable pada riwayat commit, termasuk repo besar.

## Tasks
- [x] Tulis unit/property test parser query & matcher pencarian di `crates/core/tests/search_props.rs` dan integration test pada fixture repo di `crates/shell/tests/search.rs`.
- [x] Tambah model filter pencarian (`SearchQuery`: message, author, hash prefix, path) di `crates/core/src/search.rs` dan implementasikan scan bertahap (paged & cancellable via `gix`) di `crates/shell/src/search.rs`.
- [x] Tambah search bar, pemilih filter, dan daftar virtualized yang membuka detail commit dan melompat ke baris graph.

## Definition of Done
- `cargo xtask check --core` (VPS/CI) atau `cargo xtask check` (Windows) lulus.
- `cargo test -p invoke-core --test search_props` dan `cargo test -p shell --test search` lulus untuk pencarian message, author, hash prefix, path, dan pembatalan di tengah scan.
- Pencarian tetap responsif (off UI thread, bounded hasil/halaman) tanpa memuat seluruh history ke memori.

## Out of scope
- Full-text grep isi file / pickaxe (`-S`/`-G`) dan command palette keyboard (non-goal di `docs/PRD.md`).

## Handoff notes
- Search menggunakan starting points graph (HEAD, branch lokal/remote, dan tag), commit-graph jika tersedia, serta fallback walk gix breadth-first. Walk topologis graph tetap terpisah karena fallback-nya dapat membaca seluruh ancestry sebelum halaman pertama.
- Worker mengirim progres per maksimal 256 commit dengan antrean satu halaman. Traversal menyimpan frontier dan ID yang sudah dilihat untuk deduplikasi. Metadata dan pohon dibaca per kandidat yang membutuhkannya. UI dapat membatalkan lewat token dan membuang hasil query lama.
- Filter dipilih melalui `SearchField`. Message mencari seluruh pesan. Author mencari nama atau email. Hash mencocokkan awal ID heksadesimal. Path membandingkan ID dan mode entri Git terhadap parent pertama, konsisten dengan commit detail. Root dibandingkan dengan tree kosong. Perubahan path pada batas shallow tidak dapat ditentukan. Path memakai slash dan peka huruf besar/kecil.
- Tes properti mencakup batas prefix hash, pencocokan field teks, dan perubahan identitas entri path. Ketiganya gagal saat matcher sengaja diubah, lalu matcher asli dipulihkan.
- Self-review menemukan bahwa gix dapat melewati parent shallow yang seharusnya tetap terjangkau dari ref lain. Fixture membuktikan regresi. Untuk repo shallow, walk memotong edge parent hanya dari boundary commit dan tetap mengeluarkan halaman secara inkremental.
- Verifikasi VPS lulus untuk `cargo xtask check --core` tanpa warning, `cargo test -p invoke-core --test search_props`, dan `cargo test -p shell --test search`. Target bersama memakai fingerprint artefak khusus worktree pada pengulangan gate untuk mencegah tabrakan antar-checkout.

### Windows verification checklist

Lanjutan Windows: query otomatis berjalan setelah jeda input 180 ms. UI menyimpan maksimal 500 hasil, dan pemilihan langsung membaca detail commit sambil menunggu baris graph bila halaman belum tiba. Perubahan refs mengulang pencarian dari disk. Mengganti query, keluar dari History, atau pindah tab menghentikan pembacaan sebelumnya. Tes `search_props` dan `search` masing-masing lulus 4 kasus. Verifikasi klik manual masih terbuka.

- [ ] Coba tiap mode dan amati validasi input, progres, hasil kosong, dan kegagalan baca.
- [ ] Pastikan input, scroll graph, dan pindah tab tetap responsif pada repo besar, termasuk saat batas 500 hasil tercapai.
- [ ] Uji pesan multi-baris, nama dan email penulis, hash penuh/prefix, path dengan spasi dan subdirektori, perbedaan kapitalisasi Git di Windows, serta root, delete, dan repo shallow.
- [ ] Pilih hasil dekat dan jauh. Verifikasi graph scroll ke ID yang tepat dan membuka commit detail, termasuk ketika halaman graph yang memuat ID tersebut belum tiba.
- [ ] Uji cancel, penggantian query cepat, pindah/tutup tab, serta refresh watcher setelah ref berubah. Abaikan halaman dari query atau repo lama dan ulangi scan dari disk saat referensi berubah.
- [ ] Jalankan `cargo xtask check` di Windows dan verifikasi UI manual dengan GPUI yang dipin. Pemeriksaan source di VPS menemukan bahwa tipe dan signature publik history yang dipakai app tetap sama.
