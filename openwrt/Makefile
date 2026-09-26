# SPDX-License-Identifier: MIT
#
# OpenWrt feed Makefile for tollgate-wrt (Rust edition).
#
# This Makefile follows the prebuilt-binary pattern from the Go predecessor:
# CI cross-compiles the Rust binary for each target arch, stages it into
# the package directory, and the OpenWrt SDK only packages the artifact.
#
# Binary installs to /usr/bin/tollgate-wrt — same path as the Go package.

PKG_MAKEFILE_DIR:=$(dir $(abspath $(lastword $(MAKEFILE_LIST))))

include $(TOPDIR)/rules.mk

PKG_NAME:=tollgate-wrt
PKG_VERSION:=$(if $(strip $(PACKAGE_VERSION)),$(PACKAGE_VERSION),0.1.0)
PKG_FLAGS:=overwrite

# The Rust binary + gonuts-export are built natively by CI and staged into
# $(PKG_MAKEFILE_DIR). The SDK only packages them.
PREBUILT_BIN:=$(PKG_MAKEFILE_DIR)tollgate-module-basic-rust
PREBUILT_GONUTS_EXPORT:=$(PKG_MAKEFILE_DIR)gonuts-export

PKG_BUILD_DIR:=$(BUILD_DIR)/$(PKG_NAME)-$(PKG_VERSION)

PKG_MAINTAINER:=TollGate <tollgate@tollgate.me>
PKG_LICENSE:=MIT
PKG_LICENSE_FILES:=LICENSE-MIT

PKG_BUILD_PARALLEL:=1
PKG_USE_MIPS16:=0

include $(INCLUDE_DIR)/package.mk

define Package/$(PKG_NAME)
	SECTION:=net
	CATEGORY:=Network
	TITLE:=TollGate Basic Module (Rust)
	DEPENDS:=+nodogsplash +jq
	PROVIDES:=nodogsplash-files
endef

define Package/$(PKG_NAME)/description
	TollGate Basic Module for OpenWrt — Rust edition (CDK wallet).
	Drop-in replacement for the Go binary, using CDK instead of gonuts.
endef

# ── preinst: stamp install_time, create /etc/tollgate ─────────────────
define Package/$(PKG_NAME)/preinst
#!/bin/sh

mkdir -p /etc/tollgate

if [ -f /etc/tollgate/install.json ]; then
	CURRENT_TIMESTAMP=$$(date +%s)
	jq ".install_time = $$CURRENT_TIMESTAMP" /etc/tollgate/install.json > /tmp/install.json.tmp && \
		mv /tmp/install.json.tmp /etc/tollgate/install.json || \
		echo "Warning: failed to update install_time" >&2
else
	CURRENT_TIMESTAMP=$$(date +%s)
	echo "{\"install_time\": $$CURRENT_TIMESTAMP}" > /etc/tollgate/install.json
fi

exit 0
endef

# ── postinst: portal symlink, enable + start service ──────────────────
define Package/$(PKG_NAME)/postinst
#!/bin/sh

# Create captive-portal symlink: /etc/nodogsplash/htdocs -> tollgate portal
if [ -L /etc/nodogsplash/htdocs ]; then
	echo "captive-portal symlink already exists"
else
	if [ -d /etc/nodogsplash/htdocs ]; then
		mv /etc/nodogsplash/htdocs /etc/nodogsplash/htdocs.backup
	fi
	rm -rf /etc/nodogsplash/htdocs
	ln -sf /etc/tollgate/tollgate-captive-portal-site /etc/nodogsplash/htdocs
	echo "created /etc/nodogsplash/htdocs -> /etc/tollgate/tollgate-captive-portal-site"
fi

# Enable and (re)start the service
if [ -x /etc/init.d/tollgate-wrt ]; then
	/etc/init.d/tollgate-wrt enable 2>/dev/null || true
	/etc/init.d/tollgate-wrt stop 2>/dev/null || true
	/etc/init.d/tollgate-wrt start 2>/dev/null || true
fi

exit 0
endef

# ── Build steps: copy prebuilt binary, optional UPX ───────────────────
define Build/Prepare
	mkdir -p $(PKG_BUILD_DIR)
	[ -f "$(PREBUILT_BIN)" ] || { echo "ERROR: prebuilt binary not found at $(PREBUILT_BIN). CI should stage it before invoking the SDK." >&2; exit 1; }
	$(CP) $(PREBUILT_BIN) $(PKG_BUILD_DIR)/tollgate-module-basic-rust
	if [ -f "$(PREBUILT_GONUTS_EXPORT)" ]; then \
		$(CP) $(PREBUILT_GONUTS_EXPORT) $(PKG_BUILD_DIR)/gonuts-export; \
	else \
		echo "WARNING: prebuilt gonuts-export not found at $(PREBUILT_GONUTS_EXPORT) — package will lack the migration helper." >&2; \
		touch $(PKG_BUILD_DIR)/gonuts-export; \
	fi
endef

define Build/Configure
endef

define Build/Compile
	@if [ "$(USE_UPX)" = "1" ]; then \
		if which upx >/dev/null 2>&1; then \
			ls -lh $(PKG_BUILD_DIR)/tollgate-module-basic-rust; \
			upx $(UPX_FLAGS) $(PKG_BUILD_DIR)/tollgate-module-basic-rust; \
			ls -lh $(PKG_BUILD_DIR)/tollgate-module-basic-rust; \
		else \
			echo "ERROR: USE_UPX=1 but upx not on PATH" >&2; \
			exit 1; \
		fi; \
	fi
endef

# ── Install: mirror packaging/build-ipk.sh payload (Go parity) ────────
define Package/$(PKG_NAME)/install
	$(INSTALL_DIR) $(1)/usr/bin
	$(INSTALL_BIN) $(PKG_BUILD_DIR)/tollgate-module-basic-rust $(1)/usr/bin/tollgate-wrt
	ln -s tollgate-wrt $(1)/usr/bin/tollgate
	$(INSTALL_BIN) $(PKG_MAKEFILE_DIR)../packaging/files/usr/bin/tollgate-apply-ssl $(1)/usr/bin/
	$(INSTALL_BIN) $(PKG_MAKEFILE_DIR)../packaging/files/usr/bin/tollgate-remove-ssl $(1)/usr/bin/
	$(INSTALL_BIN) $(PKG_MAKEFILE_DIR)../packaging/files/usr/bin/check_package_path $(1)/usr/bin/
	$(INSTALL_BIN) $(PKG_BUILD_DIR)/gonuts-export $(1)/usr/bin/gonuts-export

	$(INSTALL_DIR) $(1)/usr/local/bin
	$(INSTALL_BIN) $(PKG_MAKEFILE_DIR)../packaging/files/usr/local/bin/first-login-setup $(1)/usr/local/bin/

	$(INSTALL_DIR) $(1)/etc/init.d
	$(INSTALL_BIN) $(PKG_MAKEFILE_DIR)../packaging/files/etc/init.d/tollgate-wrt $(1)/etc/init.d/

	$(INSTALL_DIR) $(1)/etc/nftables.d
	$(INSTALL_DATA) $(PKG_MAKEFILE_DIR)../packaging/files/etc/nftables.d/20-nds-enforce.nft $(1)/etc/nftables.d/
	$(INSTALL_DATA) $(PKG_MAKEFILE_DIR)../packaging/files/etc/nftables.d/30-backend-firewall.nft $(1)/etc/nftables.d/
	$(INSTALL_DATA) $(PKG_MAKEFILE_DIR)../packaging/files/etc/nftables.d/31-admin-board-not-guest-reachable.nft $(1)/etc/nftables.d/

	$(INSTALL_DIR) $(1)/etc/uci-defaults
	$(INSTALL_BIN) $(PKG_MAKEFILE_DIR)../packaging/files/etc/uci-defaults/99-tollgate-setup $(1)/etc/uci-defaults/
	$(INSTALL_BIN) $(PKG_MAKEFILE_DIR)../packaging/files/etc/uci-defaults/90-tollgate-captive-portal-symlink $(1)/etc/uci-defaults/

	$(INSTALL_DIR) $(1)/etc/hotplug.d/iface
	$(INSTALL_BIN) $(PKG_MAKEFILE_DIR)../packaging/files/etc/hotplug.d/iface/95-tollgate-restart $(1)/etc/hotplug.d/iface/

	$(INSTALL_DIR) $(1)/etc/tollgate
	$(INSTALL_DIR) $(1)/etc/tollgate/ecash
	$(INSTALL_DATA) $(PKG_MAKEFILE_DIR)../packaging/files/etc/tollgate/emergency-clear.nft $(1)/etc/tollgate/

	$(INSTALL_DIR) $(1)/etc/tollgate/tollgate-captive-portal-site
	$(CP) $(PKG_MAKEFILE_DIR)../packaging/files/tollgate-captive-portal-site/* $(1)/etc/tollgate/tollgate-captive-portal-site/

	$(INSTALL_DIR) $(1)/lib/upgrade/keep.d
	$(INSTALL_DATA) $(PKG_MAKEFILE_DIR)../packaging/files/lib/upgrade/keep.d/tollgate $(1)/lib/upgrade/keep.d/
endef

FILES_$(PKG_NAME) += \
	/usr/bin/tollgate-wrt \
	/usr/bin/tollgate \
	/usr/bin/gonuts-export \
	/etc/init.d/tollgate-wrt \
	/etc/tollgate/tollgate-captive-portal-site/* \
	/lib/upgrade/keep.d/tollgate

$(eval $(call BuildPackage,$(PKG_NAME)))