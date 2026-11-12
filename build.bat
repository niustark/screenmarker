@echo off
REM Build ScreenMarker (Rust). Keep this file ASCII-only:
REM cmd.exe parses .bat in the OEM codepage, UTF-8 Chinese comments break lines.
REM Icon resource is linked in by build.rs (windres) at link time.
REM seticon (Rust, src/bin/seticon.rs) is the fallback: it rewrites all ICO
REM sizes into the PE resource section, harmless when build.rs embedded them.
setlocal
set RUSTUP_HOME=D:\Program Files\rustup
set CARGO_HOME=D:\Program Files\cargo
set PATH=%CARGO_HOME%\bin;%PATH%

pushd %~dp0
cargo build --release || goto :fail
"%~dp0target\release\seticon.exe" "%~dp0target\release\screenmarker.exe" "%~dp0app.ico" || goto :fail
echo.
echo Build OK: %~dp0target\release\screenmarker.exe
popd
exit /b 0

:fail
echo Build FAILED
popd
exit /b 1
