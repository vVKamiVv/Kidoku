@echo off
rem Builds what a release ships into target\dist\files: Setup, which is the app under another name,
rem and the command line tool. They carry their own C runtime, so a PC needs nothing installed first,
rem and build paths are rewritten, so no folder or user name from this PC goes out with them.
setlocal
cd /d "%~dp0"
if not defined CARGO_HOME set "CARGO_HOME=%USERPROFILE%\.cargo"
if not defined RUSTUP_HOME set "RUSTUP_HOME=%USERPROFILE%\.rustup"
rem shortcut: RUSTFLAGS splits at spaces, so these folders can't hold one
set "RUSTFLAGS=-C target-feature=+crt-static --remap-path-prefix=%CD%=kidoku --remap-path-prefix=%CARGO_HOME%=cargo --remap-path-prefix=%RUSTUP_HOME%=rustup"
cargo build --release --target-dir target\dist --bin vntl-gui --bin vntl || exit /b 1

set "OUT=target\dist\files"
if not exist "%OUT%" mkdir "%OUT%"
copy /y target\dist\release\vntl-gui.exe "%OUT%\Kidoku-Setup.exe" >nul || exit /b 1
copy /y target\dist\release\vntl.exe "%OUT%\vntl.exe" >nul || exit /b 1

for %%f in ("%OUT%\*.exe") do (
    findstr /m /c:"%USERNAME%" "%%f" >nul && (echo %%f holds this PC's user name& exit /b 1)
    findstr /m /i /c:"%CD%" "%%f" >nul && (echo %%f holds this folder's path& exit /b 1)
)
echo Built into %OUT%
