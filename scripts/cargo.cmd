@echo off
rem ---------------------------------------------------------------------------
rem SubLink dev helper: cargo wrapper.
rem
rem Why this exists:
rem   * MSVC toolchain needs vcvars64.bat before cargo can link/build C code
rem     (libsqlite3-sys bundles SQLite and invokes the C compiler).
rem   * In sandboxed / CI shells PATH can be empty and RUSTUP_HOME / CARGO_HOME
rem     may be unset, which makes cargo print "no default toolchain configured".
rem
rem Usage:
rem   scripts\cargo.cmd check
rem   scripts\cargo.cmd test -p sl-core
rem   scripts\cargo.cmd build --release
rem
rem NOTE: keep this file ASCII-only. cmd.exe reads .bat/.cmd using the OEM code
rem page, so non-ASCII comments break parsing.
rem ---------------------------------------------------------------------------

rem 1) Baseline PATH (a child shell may have none at all).
set "PATH=%SystemRoot%\System32;%SystemRoot%;%SystemRoot%\System32\Wbem;%SystemRoot%\System32\WindowsPowerShell\v1.0"
if exist "C:\Program Files\Git\cmd" set "PATH=%PATH%;C:\Program Files\Git\cmd"

rem 2) Rust toolchain location (override via environment if needed).
if "%RUSTUP_HOME%"=="" set "RUSTUP_HOME=%USERPROFILE%\.rustup"
if "%CARGO_HOME%"==""  set "CARGO_HOME=%USERPROFILE%\.cargo"
if exist "%CARGO_HOME%\bin" set "PATH=%CARGO_HOME%\bin;%PATH%"

rem 3) Locate vcvars64.bat.
set "VSPATH="
set "VSWHERE=%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe"
if exist "%VSWHERE%" (
  for /f "usebackq tokens=*" %%i in (`"%VSWHERE%" -latest -products * -property installationPath`) do set "VSPATH=%%i"
)
if not defined VSPATH if exist "C:\Program Files\Microsoft Visual Studio\2022\Community" set "VSPATH=C:\Program Files\Microsoft Visual Studio\2022\Community"
if not defined VSPATH if exist "C:\Program Files\Microsoft Visual Studio\2022\Enterprise" set "VSPATH=C:\Program Files\Microsoft Visual Studio\2022\Enterprise"
if not defined VSPATH if exist "C:\Program Files\Microsoft Visual Studio\2022\BuildTools" set "VSPATH=C:\Program Files\Microsoft Visual Studio\2022\BuildTools"
if not defined VSPATH if exist "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools" set "VSPATH=C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools"

if defined VSPATH (
  if exist "%VSPATH%\VC\Auxiliary\Build\vcvars64.bat" (
    call "%VSPATH%\VC\Auxiliary\Build\vcvars64.bat" >nul 2>&1
  ) else (
    echo [cargo.cmd] WARN: vcvars64.bat not found, skipping MSVC setup
  )
) else (
  echo [cargo.cmd] WARN: Visual Studio not located, skipping MSVC setup
)

cargo %*
exit /b %errorlevel%
