# 0003: GPUI for the desktop app

**Status:** Accepted, pending a spike before M5, 2026-11-19

## Context

The app is a global-hotkey palette. It must appear instantly, stream thousands of results
without jank, and sit in the tray all day at near-zero cost. The first plan suggested Tauri
(WebView2 + React).

## Decision

Build the app with **GPUI**, Zed's GPU-accelerated Rust UI framework (`gpui` 0.2.x on
crates.io, Apache-2.0), with `gpui-component` for standard widgets.

Why:
- **No WebView.** WebView2 costs ~100+ MB RAM and a noticeable cold start, which is wrong for an
  always-resident palette. GPUI renders natively via DirectX on Windows.
- **One language and one process model.** The app is plain Rust next to the engine, with no JS
  bridge and no second toolchain.
- **Proven on the hardest workload.** Zed renders huge virtualised lists and syntax-highlighted
  text with GPUI on Windows, which is exactly our results list and preview pane.

## Risks and mitigations

| Risk | Mitigation |
|---|---|
| Pre-1.0 API churn (0.2.x) | Pin an exact version. The UI talks to the engine only through `mtseek-ipc`. View models are plain Rust structs with unit tests, independent of GPUI. |
| Thin documentation | Zed's source is the documentation. Keep a `docs/gpui-notes.md` of what we learn. |
| Accessibility (screen readers) is GPUI's weakest area | Check in the spike. If it is inadequate, record it and decide before M5, not after. |
| Global hotkey, tray, borderless always-on-top window | `global-hotkey` / `tray-icon` crates. Prove each in the spike. |

## Spike (gate before M5, 1–2 days)

A throwaway borderless palette window that must show:
1. Hotkey to visible in under 100 ms (cold), and under 30 ms warm.
2. Text input with IME (CJK) and emoji.
3. A virtualised list scrolling 100k rows at 60 fps+.
4. A syntax-highlighted preview of a 10k-line file.
5. A screen-reader smoke test with Narrator.

If items 1–4 pass, GPUI is confirmed. If they don't, this ADR is superseded (likely by Tauri) with the numbers.
