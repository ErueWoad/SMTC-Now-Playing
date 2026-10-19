@echo off
setlocal
cd /d "%~dp0"
set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"
title Building SMTC Now Playing - Rust

where cargo >nul 2>&1
if %errorlevel% neq 0 goto :no_cargo

echo ===================================================
echo  Closing any running instance...
echo ===================================================
taskkill /F /IM "SMTC-Now-Playing.exe" >nul 2>&1
timeout /t 1 /nobreak >nul

echo.
echo ===================================================
echo  Compiling Standalone .EXE with Cargo Release...
echo ===================================================
cargo build --release
if %errorlevel% neq 0 goto :build_error

if not exist "dist" mkdir "dist"
copy /Y "target\release\SMTC-Now-Playing.exe" "dist\SMTC-Now-Playing.exe" >nul

echo.
echo ===================================================
echo  Refreshing Windows Icon Cache...
echo ===================================================
ie4uinit.exe -show >nul 2>&1

echo.
echo ===================================================
echo  Build Complete!
echo  Executable location: dist\SMTC-Now-Playing.exe
echo ===================================================
pause
exit /b 0

:no_cargo
echo.
echo [ERROR] Rust cargo.exe was not found on your system.
echo Please install Rust from https://rustup.rs/ and run this script again.
echo.
pause
exit /b 1

:build_error
echo.
echo [ERROR] Cargo build failed. Check the compiler output above.
echo.
pause
exit /b 1