# use PowerShell instead of sh:
set shell := ["powershell.exe", "-c"]

wasm-build:
    wasm-pack build --target web