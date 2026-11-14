// Copyright 2020-2023 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

// A silly implementation of file drop handling for Windows!

use crate::DragDropEvent;

use std::{
  cell::UnsafeCell,
  ffi::OsString,
  os::{raw::c_void, windows::ffi::OsStringExt},
  path::PathBuf,
  ptr,
  rc::Rc,
  sync::Mutex,
};

use windows::{
  Win32::{
    Foundation::{DRAGDROP_E_INVALIDHWND, HWND, LPARAM, POINT, POINTL, S_OK},
    Graphics::Gdi::ScreenToClient,
    System::{
      Com::{CoTaskMemFree, DVASPECT_CONTENT, FORMATETC, IDataObject, TYMED_HGLOBAL},
      Memory::{GlobalLock, GlobalSize, GlobalUnlock},
      Ole::{
        CF_HDROP, CF_UNICODETEXT, DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_NONE, IDropTarget,
        IDropTarget_Impl, RegisterDragDrop, ReleaseStgMedium, RevokeDragDrop,
      },
      SystemServices::MODIFIERKEYS_FLAGS,
    },
    UI::{
      Shell::{
        DragFinish, DragQueryFileW, HDROP, IShellItemArray, SHCreateShellItemArrayFromDataObject,
        SIGDN_FILESYSPATH,
      },
      WindowsAndMessaging::EnumChildWindows,
    },
  },
  core::{BOOL, implement},
};

/// The text of the last drop that carried text and no files.
static DROPPED_TEXT: Mutex<Option<String>> = Mutex::new(None);

/// The longest dropped text kept, in UTF-16 code units (4 MiB). A longer one is refused.
const MAX_DROPPED_TEXT_UNITS: usize = 2 * 1024 * 1024;

fn set_dropped_text(text: Option<String>) {
  *DROPPED_TEXT.lock().unwrap_or_else(|e| e.into_inner()) = text;
}

pub(crate) fn take_dropped_text() -> Option<String> {
  DROPPED_TEXT
    .lock()
    .unwrap_or_else(|e| e.into_inner())
    .take()
}

fn text_format() -> FORMATETC {
  FORMATETC {
    cfFormat: CF_UNICODETEXT.0,
    ptd: ptr::null_mut(),
    dwAspect: DVASPECT_CONTENT.0,
    lindex: -1,
    tymed: TYMED_HGLOBAL.0 as u32,
  }
}

/// Decodes UTF-16 up to the first NUL, since the global memory that holds the text can be larger
/// than it. Text over [`MAX_DROPPED_TEXT_UNITS`] or that is not valid UTF-16 is refused.
fn text_from_utf16(units: &[u16]) -> Option<String> {
  let units = &units[..units.len().min(MAX_DROPPED_TEXT_UNITS + 1)];
  let end = units
    .iter()
    .position(|&unit| unit == 0)
    .unwrap_or(units.len());
  if end > MAX_DROPPED_TEXT_UNITS {
    return None;
  }
  String::from_utf16(&units[..end]).ok()
}

#[derive(Default)]
pub(crate) struct DragDropController {
  drop_targets: Vec<IDropTarget>,
}

impl DragDropController {
  #[inline]
  pub(crate) fn new(hwnd: HWND, handler: Box<dyn Fn(DragDropEvent) -> bool>) -> Self {
    let mut controller = DragDropController::default();

    let handler = Rc::new(handler);

    // Enumerate child windows to find the WebView2 "window" and override!
    {
      let mut callback = |hwnd| controller.inject_in_hwnd(hwnd, handler.clone());
      let mut trait_obj: &mut dyn FnMut(HWND) -> bool = &mut callback;
      let closure_pointer_pointer: *mut c_void = unsafe { std::mem::transmute(&mut trait_obj) };
      let lparam = LPARAM(closure_pointer_pointer as _);
      unsafe extern "system" fn enumerate_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
        unsafe {
          let closure = &mut *(lparam.0 as *mut c_void as *mut &mut dyn FnMut(HWND) -> bool);
          closure(hwnd).into()
        }
      }
      let _ = unsafe { EnumChildWindows(Some(hwnd), Some(enumerate_callback), lparam) };
    }

    controller
  }

  #[inline]
  fn inject_in_hwnd(&mut self, hwnd: HWND, handler: Rc<dyn Fn(DragDropEvent) -> bool>) -> bool {
    let drag_drop_target: IDropTarget = DragDropTarget::new(hwnd, handler).into();
    if unsafe { RevokeDragDrop(hwnd) } != Err(DRAGDROP_E_INVALIDHWND.into())
      && unsafe { RegisterDragDrop(hwnd, &drag_drop_target) }.is_ok()
    {
      self.drop_targets.push(drag_drop_target);
    }

    true
  }
}

enum FileDropSource {
  HDrop(HDROP),
  ShellItemArray,
}

#[implement(IDropTarget)]
pub struct DragDropTarget {
  hwnd: HWND,
  listener: Rc<dyn Fn(DragDropEvent) -> bool>,
  cursor_effect: UnsafeCell<DROPEFFECT>,
  enter_is_valid: UnsafeCell<bool>, /* If the currently hovered item is not valid there must not be any `HoveredFileCancelled` emitted */
  /// The current drag carries text and no files.
  enter_is_text: UnsafeCell<bool>,
}

impl DragDropTarget {
  pub fn new(hwnd: HWND, listener: Rc<dyn Fn(DragDropEvent) -> bool>) -> DragDropTarget {
    Self {
      hwnd,
      listener,
      cursor_effect: DROPEFFECT_NONE.into(),
      enter_is_valid: false.into(),
      enter_is_text: false.into(),
    }
  }

  /// Whether `data_obj` offers text, without reading it.
  unsafe fn has_text(data_obj: &windows_core::Ref<'_, IDataObject>) -> bool {
    data_obj
      .as_ref()
      .is_some_and(|data_obj| unsafe { data_obj.QueryGetData(&text_format()) } == S_OK)
  }

  /// Reads the plain text (`CF_UNICODETEXT`) in `data_obj`, never any other format.
  unsafe fn read_text(data_obj: &windows_core::Ref<'_, IDataObject>) -> Option<String> {
    unsafe {
      let mut medium = data_obj.as_ref()?.GetData(&text_format()).ok()?;
      let hglobal = medium.u.hGlobal;
      let units = GlobalLock(hglobal) as *const u16;
      let text = if units.is_null() {
        None
      } else {
        let len = GlobalSize(hglobal) / 2;
        let text = text_from_utf16(std::slice::from_raw_parts(units, len));
        let _ = GlobalUnlock(hglobal);
        text
      };
      ReleaseStgMedium(&mut medium);
      text
    }
  }

  /// Calls `callback` for every dropped file-system path.
  ///
  /// Returns the source of the paths, or `None` when the data holds no file paths.
  /// A `FileDropSource::HDrop` handle must be released with `DragFinish` on drop.
  unsafe fn iterate_filenames<F>(
    data_obj: windows_core::Ref<'_, IDataObject>,
    mut callback: F,
  ) -> Option<FileDropSource>
  where
    F: FnMut(PathBuf),
  {
    unsafe {
      let drop_format = FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
      };

      match data_obj
        .as_ref()
        .expect("Received null IDataObject")
        .GetData(&drop_format)
      {
        Ok(medium) => {
          let hdrop = HDROP(medium.u.hGlobal.0 as _);

          // The second parameter (0xFFFFFFFF) instructs the function to return the item count
          let item_count = DragQueryFileW(hdrop, 0xFFFFFFFF, None);

          for i in 0..item_count {
            // Get the length of the path string NOT including the terminating null character.
            // Previously, this was using a fixed size array of MAX_PATH length, but the
            // Windows API allows longer paths under certain circumstances.
            let character_count = DragQueryFileW(hdrop, i, None) as usize;

            // Fill path_buf with the null-terminated file name
            let str_len = character_count + 1;
            let mut path_buf = vec![0; str_len];
            DragQueryFileW(hdrop, i, Some(&mut path_buf));
            callback(OsString::from_wide(&path_buf[0..character_count]).into());
          }

          Some(FileDropSource::HDrop(hdrop))
        }
        // File Explorer cannot build CF_HDROP when any dragged path is
        // MAX_PATH (260) characters or longer: GetData fails with
        // ERROR_INSUFFICIENT_BUFFER (0x8007007A), even with long paths
        // enabled. The shell item array has no such limit.
        Err(_) if Self::iterate_shell_items(&data_obj, &mut callback) => {
          Some(FileDropSource::ShellItemArray)
        }
        Err(_error) => {
          #[cfg(feature = "tracing")]
          tracing::warn!(
            "{}",
            match _error.code() {
              windows::Win32::Foundation::DV_E_FORMATETC => {
                // If the dropped item is not a file this error will occur.
                // In this case it is OK to return without taking further action.
                "Error occurred while processing dropped/hovered item: item is not a file."
              }
              _ => "Unexpected error occurred while processing dropped/hovered item.",
            }
          );
          None
        }
      }
    }
  }

  /// Reads file-system paths from the shell item array in `data_obj`.
  /// Returns true when at least one path was found.
  unsafe fn iterate_shell_items<F>(
    data_obj: &windows_core::Ref<'_, IDataObject>,
    callback: &mut F,
  ) -> bool
  where
    F: FnMut(PathBuf),
  {
    let Some(data_obj) = data_obj.as_ref() else {
      return false;
    };
    unsafe {
      let Ok(items) = SHCreateShellItemArrayFromDataObject::<_, IShellItemArray>(data_obj) else {
        return false;
      };
      let count = items.GetCount().unwrap_or(0);

      let mut found = false;
      for i in 0..count {
        let Ok(item) = items.GetItemAt(i) else {
          continue;
        };
        // Fails for items with no file-system path (virtual or device items).
        let Ok(name) = item.GetDisplayName(SIGDN_FILESYSPATH) else {
          continue;
        };
        callback(OsString::from_wide(name.as_wide()).into());
        CoTaskMemFree(Some(name.0 as _));
        found = true;
      }
      found
    }
  }
}

#[allow(non_snake_case)]
impl IDropTarget_Impl for DragDropTarget_Impl {
  fn DragEnter(
    &self,
    pDataObj: windows_core::Ref<'_, IDataObject>,
    _grfKeyState: MODIFIERKEYS_FLAGS,
    pt: &POINTL,
    pdwEffect: *mut DROPEFFECT,
  ) -> windows::core::Result<()> {
    let mut pt = POINT { x: pt.x, y: pt.y };
    let _ = unsafe { ScreenToClient(self.hwnd, &mut pt) };

    // Reset the state left by the previous drag. Otherwise a drag of neither
    // files nor text that follows an accepted one keeps `enter_is_valid` set
    // and still emits `Over` and a `Drop` with no paths.
    unsafe {
      *self.enter_is_valid.get() = false;
      *self.enter_is_text.get() = false;
      *self.cursor_effect.get() = DROPEFFECT_NONE;
    }
    set_dropped_text(None);

    let has_text = unsafe { DragDropTarget::has_text(&pDataObj) };

    let mut paths = Vec::new();
    let source = unsafe { DragDropTarget::iterate_filenames(pDataObj, |path| paths.push(path)) };

    // Files win when a drag carries both.
    let enter_is_text = source.is_none() && has_text;
    let enter_is_valid = source.is_some() || enter_is_text;

    if !enter_is_valid {
      unsafe { *pdwEffect = DROPEFFECT_NONE };
      return Ok(());
    };

    unsafe {
      *self.enter_is_valid.get() = enter_is_valid;
      *self.enter_is_text.get() = enter_is_text;
    }

    (self.listener)(DragDropEvent::Enter {
      paths,
      position: (pt.x as _, pt.y as _),
    });

    let cursor_effect = if enter_is_valid {
      DROPEFFECT_COPY
    } else {
      DROPEFFECT_NONE
    };

    unsafe {
      *pdwEffect = cursor_effect;
      *self.cursor_effect.get() = cursor_effect;
    }

    Ok(())
  }

  fn DragOver(
    &self,
    _grfKeyState: MODIFIERKEYS_FLAGS,
    pt: &POINTL,
    pdwEffect: *mut DROPEFFECT,
  ) -> windows::core::Result<()> {
    if unsafe { *self.enter_is_valid.get() } {
      let mut pt = POINT { x: pt.x, y: pt.y };
      let _ = unsafe { ScreenToClient(self.hwnd, &mut pt) };
      (self.listener)(DragDropEvent::Over {
        position: (pt.x as _, pt.y as _),
      });
    }

    unsafe { *pdwEffect = *self.cursor_effect.get() };
    Ok(())
  }

  fn DragLeave(&self) -> windows::core::Result<()> {
    if unsafe { *self.enter_is_valid.get() } {
      (self.listener)(DragDropEvent::Leave);
    }
    Ok(())
  }

  fn Drop(
    &self,
    pDataObj: windows_core::Ref<'_, IDataObject>,
    _grfKeyState: MODIFIERKEYS_FLAGS,
    pt: &POINTL,
    _pdwEffect: *mut DROPEFFECT,
  ) -> windows::core::Result<()> {
    if unsafe { *self.enter_is_valid.get() } {
      let mut pt = POINT { x: pt.x, y: pt.y };
      let _ = unsafe { ScreenToClient(self.hwnd, &mut pt) };

      let mut paths = Vec::new();
      let source = if unsafe { *self.enter_is_text.get() } {
        set_dropped_text(unsafe { DragDropTarget::read_text(&pDataObj) });
        None
      } else {
        unsafe { DragDropTarget::iterate_filenames(pDataObj, |path| paths.push(path)) }
      };
      (self.listener)(DragDropEvent::Drop {
        paths,
        position: (pt.x as _, pt.y as _),
      });

      if let Some(FileDropSource::HDrop(hdrop)) = source {
        unsafe { DragFinish(hdrop) };
      }
    }

    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::{MAX_DROPPED_TEXT_UNITS, set_dropped_text, take_dropped_text, text_from_utf16};

  fn utf16(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
  }

  #[test]
  fn dropped_text_stops_at_the_first_nul() {
    assert_eq!(
      text_from_utf16(&utf16("ls -la\0\0junk")).as_deref(),
      Some("ls -la")
    );
    assert_eq!(text_from_utf16(&utf16("uptime")).as_deref(), Some("uptime"));
    assert_eq!(text_from_utf16(&utf16("\0rest")).as_deref(), Some(""));
    assert_eq!(text_from_utf16(&[]).as_deref(), Some(""));
  }

  #[test]
  fn dropped_text_that_is_not_utf16_is_refused() {
    // A lone high surrogate.
    assert_eq!(text_from_utf16(&[0x61, 0xD800, 0x62, 0]), None);
    assert_eq!(text_from_utf16(&utf16("añ😀\0")).as_deref(), Some("añ😀"));
  }

  #[test]
  fn dropped_text_over_the_limit_is_refused() {
    let mut units = vec![0x61; MAX_DROPPED_TEXT_UNITS];
    assert_eq!(
      text_from_utf16(&units).map(|text| text.len()),
      Some(MAX_DROPPED_TEXT_UNITS)
    );
    units.push(0x61);
    assert_eq!(text_from_utf16(&units), None);
    units[MAX_DROPPED_TEXT_UNITS] = 0;
    assert!(text_from_utf16(&units).is_some());
  }

  #[test]
  fn dropped_text_is_taken_once() {
    set_dropped_text(Some("echo hi".into()));
    assert_eq!(take_dropped_text().as_deref(), Some("echo hi"));
    assert_eq!(take_dropped_text(), None);
  }
}
