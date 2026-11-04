# Vendored crates

`llama-cpp-2` 0.1.160 from crates.io (MIT OR Apache-2.0), used only by the
`jolta-ai` helper, with three changes:

- `LlamaModelParams::with_lazy_mode` (`vendor/llama-cpp-2-lazy-mode.patch`),
  so Gemma 4's per-layer embedding table stays on disk.
- `llama-cpp-sys-2` without default features, which leaves out llama.cpp's
  `common` code and the Unlicense `subprocess.h` it bundles.
- `unused` lints allowed, for the code that leaves unused.

Drop the copy once upstream has a lazy-mode setter.
