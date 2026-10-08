#!/system/bin/sh
export WGPU_BACKEND=gl
export WGPU_VALIDATION=0
export WGPU_DEBUG=0
export DIOXUS_CLI_ENABLED=true
exec "$@"
