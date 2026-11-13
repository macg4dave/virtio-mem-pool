.PHONY: help local-gate windows-gate windows-build windows-native-gate windows-status windows-deploy all-gates clean

help:
	@echo "Repository validation delegates"
	@echo "  make local-gate    - cargo xtask gate local"
	@echo "  make windows-gate  - cargo xtask windows all"
	@echo "  make windows-build       - sync, build and fetch the service from the Windows build VM"
	@echo "  make windows-native-gate - full native Windows gate on the Windows build VM"
	@echo "  make windows-status      - compare the installed service with the fetched build"
	@echo "  make windows-deploy      - install the fetched build with the site's VM settings"
	@echo "  The windows-* VM targets read WINDOWS_SITE (default site/windows-build.mk);"
	@echo "  see deploy/examples/windows-build.mk.example."
	@echo "  make all-gates     - cargo xtask gate all"
	@echo "  make clean         - remove Cargo build output"

local-gate:
	cargo xtask gate local

windows-gate:
	cargo xtask windows all

# Load the site's Windows build settings only for the targets that need them.
WINDOWS_SITE ?= site/windows-build.mk
WINDOWS_TARGETS := windows-build windows-native-gate windows-status windows-deploy
ifneq ($(filter $(WINDOWS_TARGETS),$(MAKECMDGOALS)),)
ifeq ($(wildcard $(WINDOWS_SITE)),)
$(error $(WINDOWS_SITE) not found: copy deploy/examples/windows-build.mk.example there, or set WINDOWS_SITE)
endif
include $(WINDOWS_SITE)
endif
$(WINDOWS_TARGETS): export VIRTIO_MEM_WINDOWS_SSH ?= $(WINDOWS_SSH)
$(WINDOWS_TARGETS): export VIRTIO_MEM_WINDOWS_DIR ?= $(WINDOWS_DIR)
$(WINDOWS_TARGETS): export VIRTIO_MEM_WINDOWS_ARTIFACTS ?= $(WINDOWS_ARTIFACTS)
$(WINDOWS_TARGETS): export VIRTIO_MEM_WINDOWS_CONNECT_TIMEOUT_SECONDS ?= $(WINDOWS_CONNECT_TIMEOUT_SECONDS)
$(WINDOWS_TARGETS): export VIRTIO_MEM_WINDOWS_OPERATION_TIMEOUT_SECONDS ?= $(WINDOWS_OPERATION_TIMEOUT_SECONDS)

windows-build:
	cargo xtask windows check
	cargo xtask windows sync
	cargo xtask windows build
	cargo xtask windows fetch

windows-native-gate:
	cargo xtask windows all

windows-status:
	cargo xtask windows build-status $(WINDOWS_SERVICE) --output $(VIRTIO_MEM_WINDOWS_ARTIFACTS)/build-status.json

# Restarts the Windows service in the guest with the site's VM settings.
windows-deploy:
	cargo xtask windows deploy $(WINDOWS_MANIFEST) --output $(VIRTIO_MEM_WINDOWS_ARTIFACTS)/deploy.json --apply

all-gates:
	cargo xtask gate all

clean:
	cargo clean
