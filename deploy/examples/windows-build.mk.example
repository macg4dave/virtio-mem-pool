# How make reaches the win11_gpu guest to build, test and deploy the Windows
# service (make win11-build / win11-gate / win11-status / win11-deploy).
# Values here can be overridden on the make command line or in the environment.

# SSH alias in ~/.ssh/config (address, user, identity and host key live there).
WIN11_SSH ?= virtio-mem-windows
# Repository checkout inside the guest that sources are synced to and built in.
WIN11_DIR ?= C:\Users\Dave\github\virtio_mem_windows_service
# Local folder for fetched binaries, build records and deploy evidence.
WIN11_ARTIFACTS ?= .artifacts/windows/win11_gpu
# Seconds to wait for SSH to connect, and for each remote step to finish.
WIN11_CONNECT_TIMEOUT_SECONDS ?= 10
WIN11_OPERATION_TIMEOUT_SECONDS ?= 1200
# The service settings deployed by make win11-deploy.
WIN11_MANIFEST ?= deploy/config_files/win11_gpu_settings.toml
# Built-in Windows service identity.
WIN11_SERVICE ?= VirtioMemService
