@echo off
REM Build ScreenMarker (Rust). Keep this file ASCII-only:
REM cmd.exe parses .bat in the OEM codepage, UTF-8 Chinese comments break lines.
REM Icon resource is linked in by build.rs (windres) at link time.
REM _seticon.py is only a fallback: it rewrites all ICO sizes into the PE
REM resource section, harmless when build.rs already embedded the icon.
setlocal
set RUSTUP_HOME=D:\Program Files\rustup
set CARGO_HOME=D:\Program Files\cargo
set PATH=%CARGO_HOME%\bin;%PATH%

pushd %~dp0
cargo build --release || goto :fail
python _seticon.py || goto :fail
echo.
echo Build OK: %~dp0target\release\screenmarker.exe
popd
exit /b 0

:fail
echo Build FAILED
popd
exit /b 1
