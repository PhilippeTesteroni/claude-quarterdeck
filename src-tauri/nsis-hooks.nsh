; Quarterdeck NSIS installer hooks.
;
; R-24.2: the toast-identity AppUserModelID is registered in HKCU at app startup
; (see src-tauri/src/notify.rs::register_toast_identity). Remove that key when the
; app is uninstalled so no stale toast identity is left behind. HKCU only, matching
; the installer's currentUser install mode — no elevation needed.

!macro NSIS_HOOK_POSTUNINSTALL
  DeleteRegKey HKCU "Software\Classes\AppUserModelId\pro.philippgross.quarterdeck"
!macroend
