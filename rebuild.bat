@echo off
rem ---------------------------------------------------------------------------
rem SubLink one-shot rebuild for the whole workspace:
rem   cargo build --release + refresh the desktop shortcut. Pass "clean" to
rem   run cargo clean first (full rebuild); the default is incremental.
rem
rem Usage:
rem   rebuild.bat                incremental rebuild (release only, both exes)
rem   rebuild.bat clean          full rebuild (clean + release, both exes)
rem   rebuild.bat --clean        same as clean
rem
rem Why a root-level script:
rem   * "rebuild" is a single one-button operation for the whole workspace,
rem     so it lives at the root; the dev-facing cargo wrapper stays in
rem     scripts\cargo.cmd (MSVC + cargo environment setup).
rem   * After a successful build the desktop shortcut is (re)created so it
rem     always points at the fresh sublink-gui.exe. A "clean" rebuild
rem     wipes target\, so a shortcut created before it would dangle.
rem
rem NOTE: keep this file ASCII-only. cmd.exe reads .bat/.cmd using the OEM code
rem page, so non-ASCII comments break parsing.
rem ---------------------------------------------------------------------------

setlocal EnableExtensions

set "ROOT=%~dp0"
if "%ROOT:~-1%"=="\" set "ROOT=%ROOT:~0,-1%"

set "DO_CLEAN=0"
if /i "%~1"=="clean"   set "DO_CLEAN=1"
if /i "%~1"=="--clean" set "DO_CLEAN=1"

rem --- a running GUI locks target\release\sublink-gui.exe ---------------
rem Both modes need the exe closed: cargo clean cannot remove target\release
rem while the process image holds it, and an incremental rebuild cannot
rem overwrite the running exe at link time (os error 32 / access denied).
rem Close it here instead of failing halfway. (No NSIS installer exists yet, so
rem any running sublink-gui.exe is the dev build.)
tasklist /fi "imagename eq sublink-gui.exe" 2>nul | findstr /i /c:"sublink-gui.exe" >nul
if errorlevel 1 goto :lock_check_done
echo [rebuild] closing running sublink-gui.exe ...
taskkill /im sublink-gui.exe /f >nul 2>&1
if errorlevel 1 (
  echo [rebuild] ERROR: sublink-gui.exe is running and could not be closed.
  echo [rebuild]        Quit it from the tray first, then rerun rebuild.bat.
  goto :failed
)
:lock_check_done

if "%DO_CLEAN%"=="1" (
  echo [rebuild] cargo clean ...
  call "%ROOT%\scripts\cargo.cmd" clean
  if errorlevel 1 goto :failed
) else (
  echo [rebuild] skipping cargo clean ^(incremental^)
)

echo [rebuild] cargo build --release ...
call "%ROOT%\scripts\cargo.cmd" build --release
if errorlevel 1 goto :failed

set "GUI=%ROOT%\target\release\sublink-gui.exe"
set "CLI=%ROOT%\target\release\sublink.exe"
if not exist "%GUI%" (
  echo [rebuild] ERROR: expected artifact missing: %GUI%
  goto :failed
)
if not exist "%CLI%" echo [rebuild] WARN: expected artifact missing: %CLI%

call :make_shortcut
if errorlevel 1 echo [rebuild] WARN: desktop shortcut was not refreshed.

echo.
echo [rebuild] OK
echo   CLI : %CLI%
echo   GUI : %GUI%
if defined LNK echo   Link: %LNK%

rem Keep the window open when launched by double-click (Explorer runs .bat
rem via "cmd /c"); when started from an existing console it just exits.
echo %cmdcmdline% | findstr /i /c:"cmd /c" >nul 2>&1
if not errorlevel 1 pause

endlocal
exit /b 0

:failed
echo.
echo [rebuild] FAILED - see the output above.
echo [rebuild]        Hint: LNK1181 kernel32.lib means the Windows SDK paths are
echo [rebuild]        missing - vcvars64 locates the SDK via reg.exe, which some
echo [rebuild]        sandboxed shells block. Run from a normal terminal.
echo %cmdcmdline% | findstr /i /c:"cmd /c" >nul 2>&1
if not errorlevel 1 pause
endlocal
exit /b 1

rem --- (re)create the desktop shortcut pointing at the fresh GUI exe --------
:make_shortcut
set "LNK="
if exist "%USERPROFILE%\Desktop" (
  set "LNK=%USERPROFILE%\Desktop\SubLink.lnk"
) else if exist "%USERPROFILE%\OneDrive\Desktop" (
  set "LNK=%USERPROFILE%\OneDrive\Desktop\SubLink.lnk"
)
if not defined LNK (
  echo [rebuild] WARN: no Desktop directory found, skipping shortcut.
  exit /b 1
)

where powershell >nul 2>&1
if errorlevel 1 (
  echo [rebuild] WARN: powershell not found, skipping shortcut.
  exit /b 1
)

set "ICO=%ROOT%\crates\sl-gui\icons\icon.ico"
powershell -NoProfile -ExecutionPolicy Bypass -Command "$ws = New-Object -ComObject WScript.Shell; $sc = $ws.CreateShortcut('%LNK%'); $sc.TargetPath = '%GUI%'; $sc.WorkingDirectory = '%ROOT%\target\release'; $sc.IconLocation = '%ICO%,0'; $sc.Description = 'SubLink - redirect cache directories to a RAM disk'; $sc.Save()"
if errorlevel 1 exit /b 1
echo [rebuild] shortcut refreshed: %LNK%
exit /b 0
