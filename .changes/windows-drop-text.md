---
"wry": minor
---

On Windows, accept text dropped from other apps (`CF_UNICODETEXT`) when using `WebViewBuilder::with_drag_drop_handler`. The drag emits the usual events with no paths, and the new `take_dropped_text` returns its text once. File drops are unchanged.
