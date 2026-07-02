@echo off
setlocal
rem Quarterdeck notification demo (SPEC §9 / T5 acceptance criteria).
rem
rem Fires one Idle-class toast (system default sound), waits ~5s, then fires
rem one Attention-class toast (distinct alert sound), so a human can confirm
rem both toast classes actually appear with different sounds on this machine.
rem
rem Set QUARTERDECK_FAKE_NOTIFIER=1 first to instead append the same two
rem calls to <data>/notifier-calls.jsonl without showing real toasts (R-3.2).
rem
rem Two Windows-only build/runtime quirks this script works around, both only
rem affecting `cargo run --example` (never the real packaged app):
rem   1. `tauri-plugin-notification` only uses the real `pro.philippgross.
rem      quarterdeck` AppUserModelID (R-9.3) when the exe's own directory is
rem      NOT `target\debug`/`target\release`; otherwise (e.g. `...\examples\`)
rem      it assumes "packaged" and uses that real, but-here-unregistered,
rem      AUMID, which is unreliable before an NSIS install creates the
rem      matching Start Menu shortcut. So this script copies the built exe up
rem      into `target\debug\` (not `...\debug\examples\`) before running it,
rem      which makes the plugin take its own documented dev-mode fallback to
rem      a well-known, always-registered AUMID instead.
rem   2. `cargo build --example` binaries in this crate don't get the embedded
rem      Windows manifest `tauri-build` generates for the real `quarterdeck.
rem      exe`, so they load the legacy v5 comctl32.dll and crash at load time
rem      (STATUS_ENTRYPOINT_NOT_FOUND on `TaskDialogIndirect`, a v6-only
rem      export pulled in via `tauri`). This script drops a plain
rem      side-by-side "<exe>.manifest" requesting comctl32 v6 next to the
rem      copied exe, which Windows honors without needing an embedded
rem      resource.

set "SCRIPT_DIR=%~dp0"
set "REPO_ROOT=%SCRIPT_DIR%.."

rem Defensive: on this machine cargo/rustc live in %USERPROFILE%\.cargo\bin,
rem which is not always on PATH for every shell that might invoke this .cmd.
where cargo >nul 2>nul
if errorlevel 1 set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"

pushd "%REPO_ROOT%\src-tauri"

echo Building the demo-toasts example...
cargo build --quiet --example demo_toasts
if errorlevel 1 (
  echo Build failed.
  popd
  exit /b 1
)

set "TARGET_DEBUG=%REPO_ROOT%\target\debug"
set "SRC_EXE=%TARGET_DEBUG%\examples\demo_toasts.exe"
set "RUN_EXE=%TARGET_DEBUG%\demo_toasts.exe"

copy /y "%SRC_EXE%" "%RUN_EXE%" >nul
copy /y "%SCRIPT_DIR%demo-toasts.exe.manifest" "%RUN_EXE%.manifest" >nul

echo Running Quarterdeck notification demo...
echo (fires an idle toast, waits, then fires an attention toast)
"%RUN_EXE%"
set "EXIT_CODE=%ERRORLEVEL%"

popd
endlocal & exit /b %EXIT_CODE%
