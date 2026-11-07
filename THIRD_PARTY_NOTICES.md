# Third-Party Notices

AirMirror 以 `GPL-3.0-or-later` 发布。下列组件和参考项目分别遵循其原始许可；
分发二进制或修改版时，请同时保留本文件、根目录 `LICENSE` 以及各组件目录中的
许可文本。

## AirConnect

- Project: https://github.com/philippe44/AirConnect
- License: MIT
- Usage: architectural reference for discovery, adapter boundaries and RAOP handling

## doubletake

- Project: https://github.com/omarroth/doubletake
- License: LGPL-3.0-or-later
- Usage: protocol and compatibility reference for AirPlay screen mirroring

## doubletake-rs / fpsap-rs

- Project: https://github.com/acquo/doubletake-rs
- Vendored module: `third_party/fpsap-rs`
- License: GPL-3.0-or-later
- Usage: FairPlay m2 SAP parsing, m2/m3 consistency validation, and ALAC verbatim
  audio framing reference

## fpsap-helper

- Location: `tools/fpsap-helper`
- License: GPL-3.0
- Usage: separate executable that computes the FairPlay SAP hash used by
  `fp-setup` step 2

`fpsap-helper` is distributed as a separate executable and must remain next to
the AirMirror binaries at runtime.

## OpenH264

- Project: https://github.com/cisco/openh264
- License: BSD-2-Clause
- Usage: H.264 software encoding

## playfair

- Project: https://github.com/FDH2/UxPlay/tree/master/lib/playfair
- License: GPL-3.0-or-later
- Usage: legacy FairPlay ekey decryption used by the target TV firmware

## Rust crates

The complete dependency list and license texts are available through `cargo metadata`
and `cargo deny`. A release build must include a generated dependency notice file.
