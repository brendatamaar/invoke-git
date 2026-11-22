# Bundled fonts

Both families are licensed under the SIL Open Font License 1.1 (the licence text is in the two `*LICENSE*`/`*OFL*` files here). Only the weights the design tokens use are included.

| File | Family and weight | Source | SHA-256 |
| --- | --- | --- | --- |
| `Inter-Regular.ttf` | Inter 400 | Inter 4.0 release, `extras/ttf/` (https://github.com/rsms/inter/releases/tag/v4.0) | `64f8be6e55c37e32ef03da99714bf3aa58b8f2099bfe4f759a7578e3b8291123` |
| `Inter-Medium.ttf` | Inter 500 | same | `1bda81124d6ae26ed16a7201e2bd93766af5a3b14faf79eea14d191ebbd41146` |
| `Inter-SemiBold.ttf` | Inter 600 | same | `0dc98e8aa59585394880f25ab89e6d915ad5134522e961b046ca51fad3a18255` |
| `JetBrainsMono-Regular.ttf` | JetBrains Mono 400 | JetBrains Mono 2.304 release, `fonts/ttf/` (https://github.com/JetBrains/JetBrainsMono/releases/tag/v2.304) | `a0bf60ef0f83c5ed4d7a75d45838548b1f6873372dfac88f71804491898d138f` |

They are embedded with `include_bytes!` (`crates/ui/src/fonts.rs`) and registered at start-up, so the app looks the same on a machine without them installed. To add a weight, put the file here, add it to `FILES`, and record its source and hash in this table.
